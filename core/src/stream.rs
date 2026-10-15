//! 字节流适配：将 flume 通道包装为`std::io::Read`/`std::io::Write`，供 rustshell-terminal（要求同步 Read/Write 流）消费。

use std::io;
use std::io::{Read, Write};

const CHANNEL_CAP: usize = 1024;

/// 可同步读的字节流（泵任务 到 终端）
pub struct PipeReader {
    rx: flume::Receiver<Vec<u8>>,
    cur: Vec<u8>,
    pos: usize,
    eof: bool,
}

impl PipeReader {
    pub fn new(rx: flume::Receiver<Vec<u8>>) -> Self {
        Self {
            rx,
            cur: Vec::new(),
            pos: 0,
            eof: false,
        }
    }
}

impl Read for PipeReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.pos < self.cur.len() {
                let n = (self.cur.len() - self.pos).min(out.len());
                out[..n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            if self.eof {
                return Ok(0);
            }
            match self.rx.recv() {
                Ok(chunk) => {
                    self.cur = chunk;
                    self.pos = 0;
                }
                Err(_) => {
                    self.eof = true;
                }
            }
        }
    }
}

/// 可同步写的字节流（终端 到 泵任务）
pub struct PipeWriter {
    tx: flume::Sender<Vec<u8>>,
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.tx
            .send(buf.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "session pipe closed"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl PipeWriter {
    pub fn new(tx: flume::Sender<Vec<u8>>) -> Self {
        Self { tx }
    }
}

/// 创建 (reader, sender)：sender 由输出泵填充，reader 交给 rustshell-terminal。
pub fn reader_pipe() -> (PipeReader, flume::Sender<Vec<u8>>) {
    let (tx, rx) = flume::bounded(CHANNEL_CAP);
    (PipeReader::new(rx), tx)
}

/// 创建 (writer, receiver)：writer 交给 rustshell-terminal，receiver 由输入泵消费。
pub fn writer_pipe() -> (PipeWriter, flume::Receiver<Vec<u8>>) {
    let (tx, rx) = flume::unbounded();
    (PipeWriter::new(tx), rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_pipe_roundtrip() {
        let (mut reader, tx) = reader_pipe();
        tx.send(b"hello ".to_vec()).unwrap();
        tx.send(b"world".to_vec()).unwrap();
        drop(tx);
        let mut buf = [0u8; 32];
        let n1 = reader.read(&mut buf).unwrap();
        assert_eq!(&buf[..n1], b"hello ");
        let mut rest = [0u8; 32];
        let total = {
            let mut all = Vec::new();
            all.extend_from_slice(&buf[..n1]);
            loop {
                let n = reader.read(&mut rest).unwrap();
                if n == 0 {
                    break;
                }
                all.extend_from_slice(&rest[..n]);
            }
            all
        };
        assert_eq!(&total, b"hello world");
    }

    #[test]
    fn writer_pipe_eof_on_close() {
        let (mut writer, rx) = writer_pipe();
        writer.write_all(b"abc").unwrap();
        drop(writer);
        assert_eq!(rx.recv().unwrap(), b"abc");
        assert!(rx.recv().is_err());
    }

    /// 回归：输入通道必须无界，否则消费端卡住时写端 UI 线程会阻塞在 send 上导致窗口未响应。
    #[test]
    fn writer_pipe_never_blocks_the_writer() {
        let (mut writer, rx) = writer_pipe();
        let t0 = std::time::Instant::now();
        const N: usize = 50_000;
        for i in 0..N {
            writer
                .write_all(format!("k{i}").as_bytes())
                .expect("写入不得失败");
        }
        let cost = t0.elapsed();
        assert!(
            cost < std::time::Duration::from_secs(5),
            "输入通道写端阻塞了 {cost:?}（无界通道应立即返回）"
        );
        assert_eq!(rx.len(), N, "所有输入都应完整排队");
    }
}
