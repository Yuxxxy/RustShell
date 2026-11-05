; ===========================================================================
; RustShell NSIS 模板 —— vendored from cargo-packager 0.11.8
;   upstream: crates/packager/src/package/nsis/installer.nsi
;
; 为什么不用 packager 自带的模板（2026-11-04）：
;   上游模板检测到已安装旧版本时，会弹 PageReinstall 且**默认选中「先卸载再安装」**，
;   于是 ExecWait 拉起旧卸载器；旧卸载器用 nsis_tauri_utils::FindProcess 按**进程名**
;   找 "rustshell.exe"。而 RustShell 的在线更新曾把「安装包本体」复制到
;   $INSTDIR\rustshell.exe，重启后安装程序就顶着主程序的名字在跑 —— 卸载器把它当成
;   正在运行的 RustShell，确认杀掉后安装程序自己没了：旧版卸载了，新版没装上。
;
; 本模板相对上游的改动（都刻意不碰 handlebars 占位符）：
;   1. 删掉 PageReinstall 整页、依赖它的 EarlyChecks 版本判断、以及只剩它用的
;      StrFunc 宏：升级一律**覆盖安装**，安装程序永不调用旧卸载器，也不比版本。
;   2. Section Install 里显式 SetOverwrite on，把「覆盖」写成肉眼可见的约定。
;   3. 【2026-11-05 修】两段自愈，治「无法打开要写入的文件」。
;      现象：老版本在线更新把 NSIS 安装包复制成 $INSTDIR\rustshell.exe 后直接运行它，
;      安装器于是**正顶着主程序的名字运行**，Windows 锁住这个文件 ⇒
;      File 写不进去，弹出「无法打开要写入的文件」，升级失败。
;      实测（2026-11-05，本机 cargo-packager 0.11.8 + NSIS 3.09 复现）：
;        · $EXEFILE = 文件名（完整路径是 $EXEPATH），所以
;          「$EXEFILE S== <主程序>.exe」这个判断是**成立**的；
;        · 但 Rename **失败** —— 让位的目标正是自己正在运行的映像，
;          锁写之下改名与写入都被拒；
;        · nsis_tauri_utils::FindProcess 找不到**调用者自己**，所以按名字杀进程
;          不会把安装器自己杀掉（上游的安全前提成立）。
;      修法（两段合起来才管用）：
;        A. .onInit：检测到「我正顶着主程序名字运行」就把自身复制到临时目录、
;           另起进程安装、本进程立刻退出把锁让出来（副本文件名不同 ⇒ 不递归）；
;        B. Section Install：**先杀正在运行的程序**（CheckIfAppIsRunning）→
;           再把目标改名让位（.installer.old）→ 最后才写文件。顺序反了的话，
;           程序已被改名挪走、再按进程名就找不到它了，老版本会继续在跑。
;           改名改不动就说明还有人占着，等一下重试（最多约 20 秒）；
;           首次安装（目标文件不存在）直接跳过让位循环，别白等。
;   4. CheckIfAppIsRunning 把 KillProcess 返回 2（进程已自行退出）当成功：在线更新
;      是「拉起安装器后立刻退出」，旧进程可能恰好落在 FindProcess 与 KillProcess
;      之间，上游会把这种「目标已达成」误判成「杀不掉」并 Abort。
;
; 维护约定：这是 packager 的**整份**模板，不是补丁。升级 cargo-packager 时必须
;   diff 上游新版模板后重新施加上面这几条；CI 已把版本钉在 0.11.8（见 release.yml）。
; ===========================================================================

; Set the compression algorithm.
!if "{{compression}}" == ""
  SetCompressor /SOLID lzma
!else
  SetCompressor /SOLID "{{compression}}"
!endif

Unicode true

!include MUI2.nsh
!include FileFunc.nsh
!include x64.nsh
!include WordFunc.nsh
!include "FileAssociation.nsh"
; 【RustShell】上游重复包含了两次 StrFunc.nsh，只为 PageReinstall 里那段 WiX
; 注册表扫描提供 StrCase / StrLoc。那一页已删除，这里一并去掉，免得 makensis
; 每次报两个 6010「function not referenced」警告。

!define MANUFACTURER "{{manufacturer}}"
!define PRODUCTNAME "{{product_name}}"
!define VERSION "{{version}}"
!define VERSIONWITHBUILD "{{version_with_build}}"
!define SHORTDESCRIPTION "{{short_description}}"
!define INSTALLMODE "{{install_mode}}"
!define LICENSE "{{license}}"
!define INSTALLERICON "{{installer_icon}}"
!define SIDEBARIMAGE "{{sidebar_image}}"
!define HEADERIMAGE "{{header_image}}"
!define MAINBINARYNAME "{{main_binary_name}}"
!define MAINBINARYSRCPATH "{{main_binary_path}}"
!define IDENTIFIER "{{identifier}}"
!define COPYRIGHT "{{copyright}}"
!define OUTFILE "{{out_file}}"
!define ARCH "{{arch}}"
!define PLUGINSPATH "{{additional_plugins_path}}"
!define ALLOWDOWNGRADES "{{allow_downgrades}}"
!define DISPLAYLANGUAGESELECTOR "{{display_language_selector}}"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}"
!define MANUPRODUCTKEY "Software\${MANUFACTURER}\${PRODUCTNAME}"
!define UNINSTALLERSIGNCOMMAND "{{uninstaller_sign_cmd}}"
!define ESTIMATEDSIZE "{{estimated_size}}"

Name "${PRODUCTNAME}"
BrandingText "${COPYRIGHT}"
OutFile "${OUTFILE}"

VIProductVersion "${VERSIONWITHBUILD}"
VIAddVersionKey "ProductName" "${PRODUCTNAME}"
VIAddVersionKey "FileDescription" "${SHORTDESCRIPTION}"
VIAddVersionKey "LegalCopyright" "${COPYRIGHT}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"

; Plugins path, currently exists for linux only
!if "${PLUGINSPATH}" != ""
    !addplugindir "${PLUGINSPATH}"
!endif

!if "${UNINSTALLERSIGNCOMMAND}" != ""
  !uninstfinalize '${UNINSTALLERSIGNCOMMAND}'
!endif

; Handle install mode, `perUser`, `perMachine` or `both`
!if "${INSTALLMODE}" == "perMachine"
  RequestExecutionLevel highest
!endif

!if "${INSTALLMODE}" == "currentUser"
  RequestExecutionLevel user
!endif

!if "${INSTALLMODE}" == "both"
  !define MULTIUSER_MUI
  !define MULTIUSER_INSTALLMODE_INSTDIR "${PRODUCTNAME}"
  !define MULTIUSER_INSTALLMODE_COMMANDLINE
  !if "${ARCH}" == "x64"
    !define MULTIUSER_USE_PROGRAMFILES64
  !else if "${ARCH}" == "arm64"
    !define MULTIUSER_USE_PROGRAMFILES64
  !endif
  !define MULTIUSER_INSTALLMODE_DEFAULT_REGISTRY_KEY "${UNINSTKEY}"
  !define MULTIUSER_INSTALLMODE_DEFAULT_REGISTRY_VALUENAME "CurrentUser"
  !define MULTIUSER_INSTALLMODEPAGE_SHOWUSERNAME
  !define MULTIUSER_INSTALLMODE_FUNCTION RestorePreviousInstallLocation
  !define MULTIUSER_EXECUTIONLEVEL Highest
  !include MultiUser.nsh
!endif

; installer icon
!if "${INSTALLERICON}" != ""
  !define MUI_ICON "${INSTALLERICON}"
!endif

; installer sidebar image
!if "${SIDEBARIMAGE}" != ""
  !define MUI_WELCOMEFINISHPAGE_BITMAP "${SIDEBARIMAGE}"
!endif

; installer header image
!if "${HEADERIMAGE}" != ""
  !define MUI_HEADERIMAGE
  !define MUI_HEADERIMAGE_BITMAP  "${HEADERIMAGE}"
!endif

; Define registry key to store installer language
!define MUI_LANGDLL_REGISTRY_ROOT "HKCU"
!define MUI_LANGDLL_REGISTRY_KEY "${MANUPRODUCTKEY}"
!define MUI_LANGDLL_REGISTRY_VALUENAME "Installer Language"

; Installer pages, must be ordered as they appear
; 1. Welcome Page
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
!insertmacro MUI_PAGE_WELCOME

; 2. License Page (if defined)
!if "${LICENSE}" != ""
  !define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
  !insertmacro MUI_PAGE_LICENSE "${LICENSE}"
!endif

; 3. Install mode (if it is set to `both`)
!if "${INSTALLMODE}" == "both"
  !define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
  !insertmacro MULTIUSER_PAGE_INSTALLMODE
!endif


; 4. 【RustShell】没有「重新安装 / 先卸载」这一页。
;
; 上游在这里挂了 `Page custom PageReinstall PageLeaveReinstall`，检测到旧版本时
; 默认选中「卸载后安装」，并在 PageLeaveReinstall 里 ExecWait 旧卸载器。
; 那条路径会把正在运行的安装程序（老版本在线更新把安装包写成了 rustshell.exe）
; 当成「正在运行的 RustShell」杀掉，结果是「卸载了，却没装上」。
;
; 覆盖安装不需要卸载器：文件由 Section Install 直接覆盖，注册表 / 快捷方式 /
; 卸载器由本次安装重新写一遍，用户数据与配置目录一律不动。
; 卸载器仍然生成，但**只有用户从「设置 → 应用」主动卸载时才会运行**。
; 因此这里不注册任何 Page，PageReinstall / PageLeaveReinstall /
; PageReinstallUpdateSelection 三个函数也一并删除。

; 5. Choose install directoy page
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
!insertmacro MUI_PAGE_DIRECTORY

; 6. Start menu shortcut page
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
Var AppStartMenuFolder
!insertmacro MUI_PAGE_STARTMENU Application $AppStartMenuFolder

; 7. Installation page
!insertmacro MUI_PAGE_INSTFILES

; 8. Finish page
;
; Don't auto jump to finish page after installation page,
; because the installation page has useful info that can be used debug any issues with the installer.
!define MUI_FINISHPAGE_NOAUTOCLOSE
; Use show readme button in the finish page as a button create a desktop shortcut
!define MUI_FINISHPAGE_SHOWREADME
!define MUI_FINISHPAGE_SHOWREADME_TEXT "$(createDesktop)"
!define MUI_FINISHPAGE_SHOWREADME_FUNCTION CreateDesktopShortcut
; Show run app after installation.
!define MUI_FINISHPAGE_RUN "$INSTDIR\${MAINBINARYNAME}.exe"
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
!insertmacro MUI_PAGE_FINISH

; Uninstaller Pages
; 1. Confirm uninstall page
{{#if appdata_paths}}
Var DeleteAppDataCheckbox
Var DeleteAppDataCheckboxState
!define /ifndef WS_EX_LAYOUTRTL         0x00400000
!define MUI_PAGE_CUSTOMFUNCTION_SHOW un.ConfirmShow
Function un.ConfirmShow
    FindWindow $1 "#32770" "" $HWNDPARENT ; Find inner dialog
    ${If} $(^RTL) == 1
      System::Call 'USER32::CreateWindowEx(i${__NSD_CheckBox_EXSTYLE}|${WS_EX_LAYOUTRTL},t"${__NSD_CheckBox_CLASS}",t "$(deleteAppData)",i${__NSD_CheckBox_STYLE},i 50,i 100,i 400, i 25,i$1,i0,i0,i0)i.s'
    ${Else}
      System::Call 'USER32::CreateWindowEx(i${__NSD_CheckBox_EXSTYLE},t"${__NSD_CheckBox_CLASS}",t "$(deleteAppData)",i${__NSD_CheckBox_STYLE},i 0,i 100,i 400, i 25,i$1,i0,i0,i0)i.s'
    ${EndIf}
    Pop $DeleteAppDataCheckbox
    SendMessage $HWNDPARENT ${WM_GETFONT} 0 0 $1
    SendMessage $DeleteAppDataCheckbox ${WM_SETFONT} $1 1
FunctionEnd
!define MUI_PAGE_CUSTOMFUNCTION_LEAVE un.ConfirmLeave
Function un.ConfirmLeave
    SendMessage $DeleteAppDataCheckbox ${BM_GETCHECK} 0 0 $DeleteAppDataCheckboxState
FunctionEnd
{{/if}}
!insertmacro MUI_UNPAGE_CONFIRM

; 2. Uninstalling Page
!insertmacro MUI_UNPAGE_INSTFILES

;Languages
{{#each languages}}
!insertmacro MUI_LANGUAGE "{{this}}"
{{/each}}
!insertmacro MUI_RESERVEFILE_LANGDLL
{{#each language_files}}
  !include "{{this}}"
{{/each}}

!macro SetContext
  !if "${INSTALLMODE}" == "currentUser"
    SetShellVarContext current
  !else if "${INSTALLMODE}" == "perMachine"
    SetShellVarContext all
  !endif

  ${If} ${RunningX64}
    !if "${ARCH}" == "x64"
      SetRegView 64
    !else if "${ARCH}" == "arm64"
      SetRegView 64
    !else
      SetRegView 32
    !endif
  ${EndIf}
!macroend

Var PassiveMode
Function .onInit
  ${GetOptions} $CMDLINE "/P" $PassiveMode
  IfErrors +2 0
    StrCpy $PassiveMode 1

  !if "${DISPLAYLANGUAGESELECTOR}" == "true"
    !insertmacro MUI_LANGDLL_DISPLAY
  !endif

  !insertmacro SetContext

  ; ── 先把安装目录定下来 ──────────────────────────────────────────────────
  ; 顺序要紧：下面的「自愈 A」要把 /D=<安装目录> 转给副本，$INSTDIR 这时
  ; 必须已经是最终值（注册表里上次装的位置，或默认位置）。
  ${If} $INSTDIR == ""
    ; Set default install location
    !if "${INSTALLMODE}" == "perMachine"
      ${If} ${RunningX64}
        !if "${ARCH}" == "x64"
          StrCpy $INSTDIR "$PROGRAMFILES64\${PRODUCTNAME}"
        !else if "${ARCH}" == "arm64"
          StrCpy $INSTDIR "$PROGRAMFILES64\${PRODUCTNAME}"
        !else
          StrCpy $INSTDIR "$PROGRAMFILES\${PRODUCTNAME}"
        !endif
      ${Else}
        StrCpy $INSTDIR "$PROGRAMFILES\${PRODUCTNAME}"
      ${EndIf}
    !else if "${INSTALLMODE}" == "currentUser"
      StrCpy $INSTDIR "$LOCALAPPDATA\${PRODUCTNAME}"
    !endif

    Call RestorePreviousInstallLocation
  ${EndIf}


  !if "${INSTALLMODE}" == "both"
    !insertmacro MULTIUSER_INIT
  !endif

  ; ── 自愈 A：安装器正顶着主程序的名字运行 ────────────────────────────────
  ; 老版本在线更新把 NSIS 安装包复制成了 $INSTDIR\<主程序>.exe 并直接运行它。
  ; 这时 NSIS 安装器**自己就打开着这个文件**（它要从自己的映像里读 payload），
  ; 这个句柄没有 FILE_SHARE_DELETE ⇒ 不但写不进去，**连改名都被拒**
  ; （实测 Rename 直接返回 FAIL，File 报「无法打开要写入的文件」）。
  ; 光靠「先改名让位」救不了，因为让位的目标正是自己正在运行的映像。
  ; 解法：把自身复制到临时目录、另起进程去装，本进程**立刻退出**把锁让出来。
  ; 副本的文件名与主程序不同 ⇒ 副本不会再进这一段，不会递归。
  ; 参数原样转给副本：/S 静默、/P passive、/R 装完拉起新主程序、/D= 安装目录。
  ${If} "$EXEFILE" S== "${MAINBINARYNAME}.exe"
    ; 复制一份到临时目录再装。复制失败就往下走正常流程（至少让用户看到
    ; 原始错误，而不是静默退出、什么都不发生）。
    ClearErrors
    CopyFiles "$EXEPATH" "$TEMP\${MAINBINARYNAME}-setup.exe"
    IfErrors self_heal_copy_failed
    ; 参数原样转给副本：/S 静默、/P passive、/R 装完拉起新主程序。
    ; /D= 必须是**最后一个**参数且不能带引号（NSIS 的规矩），所以放最后。
    StrCpy $R0 ""
    ${GetOptions} $CMDLINE "/S" $R1
    IfErrors +2 0
      StrCpy $R0 "/S"
    ${If} $PassiveMode = 1
      StrCpy $R0 "$R0 /P"
    ${EndIf}
    ${GetOptions} $CMDLINE "/R" $R1
    IfErrors +2 0
      StrCpy $R0 "$R0 /R"
    ; ExecShell 而不是 Exec：Exec/nsExec 会**等子进程结束**，而子进程要等
    ; 本进程退出才能拿到写权限 —— 那样就是死等。
    ExecShell "open" "$TEMP\${MAINBINARYNAME}-setup.exe" "$R0 /D=$INSTDIR"
    Quit
    self_heal_copy_failed:
  ${EndIf}
FunctionEnd


; 【RustShell】上游的 Section EarlyChecks 用 PageReinstall 写的 $R0 判断「静默安装
; 禁止降级」。覆盖安装不比较版本，那个 $R0 不再有人赋值，整节一并删除。
; 仍保留 preinstall_section 的插入点，将来需要自定义前置逻辑时不用再改模板。
{{#if preinstall_section}}
{{unescape_newlines preinstall_section}}
{{/if}}

!macro CheckIfAppIsRunning
  ; nsis_tauri_utils 按**进程名**匹配并跳过调用者自己的 PID。安装器这一侧调用它
  ; 是为了关掉真正在跑的 RustShell（否则 exe 被锁、File 写不进去）；卸载器那一侧
  ; 只在用户主动卸载时才会走到。安装流程里不再有第二个进程来按名字杀进程。
  nsis_tauri_utils::FindProcess "${MAINBINARYNAME}.exe"
  Pop $R0
  ${If} $R0 = 0
      IfSilent kill 0
      ${IfThen} $PassiveMode != 1 ${|} MessageBox MB_OKCANCEL "$(appRunningOkKill)" IDOK kill IDCANCEL cancel ${|}
      kill:
        nsis_tauri_utils::KillProcess "${MAINBINARYNAME}.exe"
        Pop $R0
        Sleep 500
        ${If} $R0 = 0
          Goto app_check_done
        ${ElseIf} $R0 = 2
          ; 【RustShell】返回 2 = 没找到进程。在线更新里本程序在拉起安装器后
          ; 立刻退出，完全可能刚好落在 FindProcess 与 KillProcess 之间；此时目标
          ; 其实已经达成（exe 已解锁），上游却把它当「杀不掉」直接 Abort，
          ; 结果是「装到一半停住」。这里按成功处理。
          Goto app_check_done
        ${Else}
          IfSilent silent ui
          silent:
            System::Call 'kernel32::AttachConsole(i -1)i.r0'
            ${If} $0 != 0
              System::Call 'kernel32::GetStdHandle(i -11)i.r0'
              System::call 'kernel32::SetConsoleTextAttribute(i r0, i 0x0004)' ; set red color
              FileWrite $0 "$(appRunning)$\n"
            ${EndIf}
            Abort
          ui:
            Abort "$(failedToKillApp)"
        ${EndIf}
      cancel:
        Abort "$(appRunning)"
  ${EndIf}
  app_check_done:
!macroend

Section Install
  SetOutPath $INSTDIR

  ; 覆盖安装：新文件直接替换旧文件，绝不先删、绝不先跑卸载器。
  ; 写在这里而不是只靠 NSIS 默认值，是为了让「升级 = 覆盖」成为显式约定。
  SetOverwrite on

  ; ── 先把真正在跑的主程序关掉 ────────────────────────────────────────────
  ; 顺序要紧：**先杀进程、再让位改名、最后才写文件**。
  ; 反过来的话，程序已经被改名挪走（进程名还叫 rustshell.exe，映像文件却已经
  ; 变成 .installer.old），再按进程名找不一定还能找到它 ⇒ 老版本继续在跑，
  ; 用户以为没升级。
  !insertmacro CheckIfAppIsRunning

  ; ── 让位：把目标主程序挪开，保证下面的 File 写得进去 ────────────────────
  ; Windows 锁住**正在运行**的映像：写不进去（报「无法打开要写入的文件」）。
  ; 但「改名」是可以的 —— 映像加载器是以 FILE_SHARE_DELETE 打开的 ⇒
  ; 先把旧文件改名让出路径，再往空出来的路径写新文件，就是一次干净的覆盖。
  ; 唯一改名也失败的情况是「安装器自己正顶着这个文件」（NSIS 要从自己的映像
  ; 里读 payload，那个句柄没有 FILE_SHARE_DELETE）—— 那种情况由 .onInit 的
  ; 「换副本安装」处理。
  ; 改名出来的 .installer.old 是**旧主程序**，不是安装器残留；
  ; 应用启动时的 cleanup_stale_backup() 会把它和 <exe>.old 一起清掉。
  ;
  ; 首次安装（目标文件压根不存在）直接跳过，别白等 20 秒。
  IfFileExists "$INSTDIR\${MAINBINARYNAME}.exe" 0 room_done
  StrCpy $R1 0
  make_room:
    ClearErrors
    Delete "$INSTDIR\${MAINBINARYNAME}.installer.old"
    ClearErrors
    Rename "$INSTDIR\${MAINBINARYNAME}.exe" "$INSTDIR\${MAINBINARYNAME}.installer.old"
    IfErrors room_busy room_ok
    room_ok:
      Goto room_done
    room_busy:
    ${If} $R1 >= 40
      ; 20 秒还腾不出位置：交给下面的 File 去报错（NSIS 会弹出原始错误信息，
      ; 用户至少知道「文件被占用」，而不是静默装一半）。
      Goto room_done
    ${EndIf}
    IntOp $R1 $R1 + 1
    Sleep 500
    Goto make_room
  room_done:

  ; Copy main executable
  File "${MAINBINARYSRCPATH}"

  ; Create resources directory structure
  {{#each resources_dirs}}
    CreateDirectory "$INSTDIR\\{{this}}"
  {{/each}}

  ; Copy resources
  {{#each resources}}
    File /a "/oname={{this}}" "{{@key}}"
  {{/each}}

  ; Copy external binaries
  {{#each binaries}}
    File /a "/oname={{this}}" "{{@key}}"
  {{/each}}

  ; Create file associations
  {{#each file_associations as |association| ~}}
    {{#each association.extensions as |ext| ~}}
       !insertmacro APP_ASSOCIATE "{{ext}}" "{{or association.name ext}}" "{{association-description association.description ext}}" "$INSTDIR\${MAINBINARYNAME}.exe,0" "Open with ${PRODUCTNAME}" "$INSTDIR\${MAINBINARYNAME}.exe $\"%1$\""
    {{/each}}
  {{/each}}

  ; Register deep links
  {{#each deep_link_protocols as |protocol| ~}}
    WriteRegStr SHCTX "Software\Classes\\{{protocol}}" "URL Protocol" ""
    WriteRegStr SHCTX "Software\Classes\\{{protocol}}" "" "URL:${BUNDLEID} protocol"
    WriteRegStr SHCTX "Software\Classes\\{{protocol}}\DefaultIcon" "" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\",0"
    WriteRegStr SHCTX "Software\Classes\\{{protocol}}\shell\open\command" "" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\" $\"%1$\""
  {{/each}}

  ; Create uninstaller
  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; Save $INSTDIR in registry for future installations
  WriteRegStr SHCTX "${MANUPRODUCTKEY}" "" $INSTDIR

  !if "${INSTALLMODE}" == "both"
    ; Save install mode to be selected by default for the next installation such as updating
    ; or when uninstalling
    WriteRegStr SHCTX "${UNINSTKEY}" $MultiUser.InstallMode 1
  !endif

  ; Registry information for add/remove programs
  WriteRegStr SHCTX "${UNINSTKEY}" "DisplayName" "${PRODUCTNAME}"
  WriteRegStr SHCTX "${UNINSTKEY}" "DisplayIcon" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\""
  WriteRegStr SHCTX "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr SHCTX "${UNINSTKEY}" "Publisher" "${MANUFACTURER}"
  WriteRegStr SHCTX "${UNINSTKEY}" "InstallLocation" "$\"$INSTDIR$\""
  WriteRegStr SHCTX "${UNINSTKEY}" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteRegDWORD SHCTX "${UNINSTKEY}" "NoModify" "1"
  WriteRegDWORD SHCTX "${UNINSTKEY}" "NoRepair" "1"
  WriteRegDWORD SHCTX "${UNINSTKEY}" "EstimatedSize" "${ESTIMATEDSIZE}"

  ; Create start menu shortcut (GUI)
  !insertmacro MUI_STARTMENU_WRITE_BEGIN Application
    Call CreateStartMenuShortcut
  !insertmacro MUI_STARTMENU_WRITE_END

  ; Create shortcuts for silent and passive installers, which
  ; can be disabled by passing `/NS` flag
  ; GUI installer has buttons for users to control creating them
  IfSilent check_ns_flag 0
  ${IfThen} $PassiveMode == 1 ${|} Goto check_ns_flag ${|}
  Goto shortcuts_done
  check_ns_flag:
    ${GetOptions} $CMDLINE "/NS" $R0
    IfErrors 0 shortcuts_done
      Call CreateDesktopShortcut
      Call CreateStartMenuShortcut
  shortcuts_done:

  ; Auto close this page for passive mode
  ${IfThen} $PassiveMode == 1 ${|} SetAutoClose true ${|}
SectionEnd

Function .onInstSuccess
  ; Check for `/R` flag only in silent and passive installers because
  ; GUI installer has a toggle for the user to (re)start the app
  IfSilent check_r_flag 0
  ${IfThen} $PassiveMode == 1 ${|} Goto check_r_flag ${|}
  Goto run_done
  check_r_flag:
    ${GetOptions} $CMDLINE "/R" $R0
    IfErrors run_done 0
      Exec '"$INSTDIR\${MAINBINARYNAME}.exe"'
  run_done:
FunctionEnd

Function un.onInit
  !insertmacro SetContext

  !if "${INSTALLMODE}" == "both"
    !insertmacro MULTIUSER_UNINIT
  !endif

  !insertmacro MUI_UNGETLANGUAGE
FunctionEnd

Section Uninstall
  ; 【RustShell】安装程序永不 ExecWait 本卸载器；只由用户在「设置 → 应用」里主动触发。
  !insertmacro CheckIfAppIsRunning

  ; Delete the app directory and its content from disk
  ; Copy main executable
  Delete "$INSTDIR\${MAINBINARYNAME}.exe"

  ; Delete resources
  {{#each resources}}
    Delete "$INSTDIR\\{{this}}"
  {{/each}}

  ; Delete external binaries
  {{#each binaries}}
    Delete "$INSTDIR\\{{this}}"
  {{/each}}

  ; Delete app associations
  {{#each file_associations as |association| ~}}
    {{#each association.ext as |ext| ~}}
      !insertmacro APP_UNASSOCIATE "{{ext}}" "{{or association.name ext}}"
    {{/each}}
  {{/each}}

  ; Delete deep links
  {{#each deep_link_protocols as |protocol| ~}}
    ReadRegStr $R7 SHCTX "Software\Classes\\{{protocol}}\shell\open\command" ""
    !if $R7 == "$\"$INSTDIR\${MAINBINARYNAME}.exe$\" $\"%1$\""
      DeleteRegKey SHCTX "Software\Classes\\{{protocol}}"
    !endif
  {{/each}}

  ; Delete uninstaller
  Delete "$INSTDIR\uninstall.exe"

  {{#each resources_dirs}}
  RMDir /REBOOTOK "$INSTDIR\\{{this}}"
  {{/each}}
  RMDir "$INSTDIR"

  ; Remove start menu shortcut
  !insertmacro MUI_STARTMENU_GETFOLDER Application $AppStartMenuFolder
  Delete "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk"
  RMDir "$SMPROGRAMS\$AppStartMenuFolder"

  ; Remove desktop shortcuts
  Delete "$DESKTOP\${PRODUCTNAME}.lnk"

  ; Remove registry information for add/remove programs
  !if "${INSTALLMODE}" == "both"
    DeleteRegKey SHCTX "${UNINSTKEY}"
  !else if "${INSTALLMODE}" == "perMachine"
    DeleteRegKey HKLM "${UNINSTKEY}"
  !else
    DeleteRegKey HKCU "${UNINSTKEY}"
  !endif

  DeleteRegValue HKCU "${MANUPRODUCTKEY}" "Installer Language"

  ; Delete app data
  {{#if appdata_paths}}
  ${If} $DeleteAppDataCheckboxState == 1
      SetShellVarContext current
      {{#each appdata_paths}}
      RmDir /r "{{unescape_dollar_sign this}}"
      {{/each}}
  ${EndIf}
  {{/if}}

  ${GetOptions} $CMDLINE "/P" $R0
  IfErrors +2 0
    SetAutoClose true
SectionEnd

Function RestorePreviousInstallLocation
  ReadRegStr $4 SHCTX "${MANUPRODUCTKEY}" ""
  StrCmp $4 "" +2 0
    StrCpy $INSTDIR $4
FunctionEnd

Function SkipIfPassive
  ${IfThen} $PassiveMode == 1  ${|} Abort ${|}
FunctionEnd

Function CreateDesktopShortcut
  CreateShortcut "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  ApplicationID::Set "$DESKTOP\${PRODUCTNAME}.lnk" "${IDENTIFIER}"
FunctionEnd

Function CreateStartMenuShortcut
  CreateDirectory "$SMPROGRAMS\$AppStartMenuFolder"
  CreateShortcut "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  ApplicationID::Set "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk" "${IDENTIFIER}"
FunctionEnd
