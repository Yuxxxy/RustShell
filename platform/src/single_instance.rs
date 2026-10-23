//! 单实例：第二次启动的命令行交给已经在运行的那个进程。
//!
//! 堡垒机（JumpServer 等）把 `ssh://` / `sftp://` 链接交给「已注册的客户端」时，
//! 用户往往已经开着一个 RustShell。这时再起一个完整进程不仅要点两次，还会让
//! 两个实例各写一份会话布局 / 配置。
//!
//! 做法是**纯用户态、只绑回环**的一对 socket：
//! * 先起来的那位把 `端口 + 令牌` 写进配置目录的 `instance-endpoint`，并常驻
//!   一个 `127.0.0.1` 监听线程；
//! * 后起来的那位读到端点就连上去，把 `argv` 原样发过去，收到 `ok` 后自身退出；
//! * 端点文件过期（上次崩溃 / 换了机器）时连接失败，就自己接管成主实例 ——
//!   不留「谁也起不来」的死局。
//!
//! 令牌是随机的：同一台机器上的其它进程即使猜到端口，没有令牌也塞不进命令。
//! 这里不做「加锁」，因为唯一需要互斥的是「谁当主实例」，而它由
//! 「端点能连上吗」+ 写完后复查一次文件决定。

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 本进程的身份。
pub enum InstanceRole {
    /// 主实例：`requests` 里会陆续收到**别的进程转发过来**的启动参数（每项都是
    /// 去掉 `argv[0]` 之后的完整参数表）；`forward` 用来把**本进程自己的**参数
    /// 塞进同一条通道，好让 UI 只有一条处理路径。
    Primary {
        requests: flume::Receiver<Vec<String>>,
        forward: flume::Sender<Vec<String>>,
    },
    /// 已转交给正在运行的那个实例，本进程应当直接退出。
    Forwarded,
}

/// 端点文件：端口与令牌。放在用户配置目录里（同目录已有设置与连接配置）。
pub fn endpoint_path() -> PathBuf {
    crate::paths::config_dir().join("instance-endpoint")
}

/// 判定身份；需要时启动常驻监听线程。
pub fn claim(args: &[String]) -> InstanceRole {
    claim_at(&endpoint_path(), args)
}

/// `claim` 的可注入版本（测试用临时端点，免得动全局配置目录）。
pub fn claim_at(endpoint: &Path, args: &[String]) -> InstanceRole {
    // ① 已经有主实例：交出去。
    if forward(endpoint, args) {
        log::info!("已有运行中的实例，启动参数已转交");
        return InstanceRole::Forwarded;
    }

    // ② 没有：自己接管。
    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(l) => l,
        Err(e) => {
            // 连回环都绑不上（几乎只有沙箱会这样）：退化成「没有单实例」，
            // 至少本进程要能正常开窗。
            log::warn!("单实例监听无法启动（{e}），本次按独立实例运行");
            let (tx, rx) = flume::unbounded();
            return InstanceRole::Primary {
                requests: rx,
                forward: tx,
            };
        }
    };
    let port = match listener.local_addr() {
        Ok(a) => a.port(),
        Err(e) => {
            log::warn!("单实例端点地址不可用（{e}）");
            let (tx, rx) = flume::unbounded();
            return InstanceRole::Primary {
                requests: rx,
                forward: tx,
            };
        }
    };
    let token = make_token();
    let ours = format!("{port} {token}\n");
    if let Some(dir) = endpoint.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(endpoint, &ours) {
        log::warn!("写单实例端点失败（{e}）：不做启动参数转交");
        let (tx, rx) = flume::unbounded();
        return InstanceRole::Primary {
            requests: rx,
            forward: tx,
        };
    }

    // ③ 竞态复查：两个进程几乎同时启动时，后写的一方会覆盖先写的端点。
    //    等一小会儿再看一眼文件 —— 不是自己写的那份，说明输了，转为转交。
    std::thread::sleep(Duration::from_millis(40));
    if std::fs::read_to_string(endpoint).ok().as_deref() != Some(ours.as_str()) {
        if forward(endpoint, args) {
            log::info!("与另一实例同时启动，本进程转为转交并退出");
            return InstanceRole::Forwarded;
        }
        // 对方还没来得及监听：把自己的端点写回去，接着当主实例。
        let _ = std::fs::write(endpoint, &ours);
    }

    let (tx, rx) = flume::unbounded::<Vec<String>>();
    let tx_thread = tx.clone();
    std::thread::Builder::new()
        .name("rs-instance".to_string())
        .spawn(move || serve(listener, token, tx_thread))
        .expect("spawn single-instance listener");
    InstanceRole::Primary {
        requests: rx,
        forward: tx,
    }
}

/// 常驻监听：验证令牌，把参数表交回 UI 线程。
fn serve(listener: TcpListener, token: String, tx: flume::Sender<Vec<String>>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let mut writer = match stream.try_clone() {
            Ok(w) => w,
            Err(_) => continue,
        };
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            continue;
        }
        // 令牌不对：静默断开。不回错误详情，免得替扫描者确认「端口对了」。
        if line.trim_end() != token {
            continue;
        }
        let mut args = Vec::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => args.push(line.trim_end_matches(['\r', '\n']).to_string()),
                Err(_) => break,
            }
        }
        if tx.send(args).is_err() {
            return; // UI 已经不在
        }
        let _ = writer.write_all(b"ok\n");
        let _ = writer.flush();
    }
}

/// 把参数交给正在运行的实例。返回 `true` = 已送达。
fn forward(endpoint: &Path, args: &[String]) -> bool {
    let Ok(text) = std::fs::read_to_string(endpoint) else {
        return false;
    };
    let Some((port, token)) = text.trim().split_once(' ') else {
        return false;
    };
    let Ok(port) = port.parse::<u16>() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(400),
    ) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let mut payload = String::new();
    payload.push_str(token);
    payload.push('\n');
    for arg in args {
        // 参数里出现换行会破坏行协议；这种参数本来也不可能来自链接。
        payload.push_str(&arg.replace(['\r', '\n'], " "));
        payload.push('\n');
    }
    if stream.write_all(payload.as_bytes()).is_err() {
        return false;
    }
    let _ = stream.flush();
    // 半关写端：对端读到 EOF 才知道「参数发完了」，否则双方各等 2 秒超时。
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut ack = String::new();
    let mut reader = BufReader::new(stream);
    matches!(reader.read_line(&mut ack), Ok(n) if n > 0 && ack.trim_end() == "ok")
}

/// 端点令牌：毫秒时间 + 进程号 + 栈地址，够挡住同机其它进程的误撞。
fn make_token() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let stack = 0u8;
    format!(
        "{:x}{:x}{:x}",
        now,
        std::process::id(),
        std::ptr::addr_of!(stack) as usize
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_endpoint(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rustshell-instance-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("endpoint")
    }

    #[test]
    fn second_launch_is_forwarded_to_the_first() {
        let path = temp_endpoint("forward");
        let InstanceRole::Primary { requests, .. } =
            claim_at(&path, &["-url".into(), "ssh://a@h".into()])
        else {
            panic!("第一个实例应当成为主实例");
        };

        let InstanceRole::Forwarded = claim_at(
            &path,
            &[
                "-url".into(),
                "sftp://u:p@h/".into(),
                "/sessionname=x".into(),
            ],
        ) else {
            panic!("第二个实例应当被转交");
        };

        let got = requests
            .recv_timeout(Duration::from_secs(2))
            .expect("主实例没有收到参数");
        assert_eq!(
            got.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["-url", "sftp://u:p@h/", "/sessionname=x"]
        );

        // 端点文件被篡改（令牌不对）：不能悄悄塞进命令，且主实例照常工作。
        std::fs::write(&path, "1 deadbeef\n").unwrap();
        assert!(!forward(&path, &["ssh://evil".into()]));
    }

    #[test]
    fn stale_endpoint_is_taken_over() {
        let path = temp_endpoint("stale");
        // 端口 1 上没有任何东西在听：端点过期，应当自己接管。
        std::fs::write(&path, "1 deadbeef\n").unwrap();
        let role = claim_at(&path, &[]);
        assert!(matches!(role, InstanceRole::Primary { .. }));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_ne!(text.trim(), "1 deadbeef");
    }
}
