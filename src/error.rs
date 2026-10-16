//! 统一的错误类型与退出流程。
//!
//! # 为什么需要这个模块
//!
//! bazaar 会长时间霸占用户的终端（raw mode + 备用屏 + 隐藏光标）。只要有一条退出路径忘了
//! 还回去，用户拿到的就是一个没有回显、没有光标、连 Ctrl+C 都退不出的终端——这比程序本身
//! 崩溃更让人难受。而 `Cargo.toml` 里 `panic = "abort"` 意味着 **析构函数不会执行**，
//! 所以"把恢复逻辑写在 `Drop` 里"这条路从根上就不可靠。
//!
//! 本模块把退出收敛成三种情况，每种都有确定行为：
//!
//! | 情况 | 触发方式 | 行为 |
//! |---|---|---|
//! | 可恢复错误 | `Error` 经 `?` 冒泡到 [`run`] | 打印错误链 → 按 [`ExitCode`] 退出 |
//! | panic | `unwrap()` / 下标越界 / 断言失败 | panic hook 先恢复终端，再交给默认 hook 打印 |
//! | 致命信号 | SIGINT / SIGTERM / SIGHUP / SIGQUIT | 处理器恢复终端 → 重置为默认处置 → 重新抛出 |
//!
//! 所有恢复动作只依赖 `write(2)` / `tcsetattr` / `kill(2)` 这类不分配、不加锁的调用，
//! 因此可以安全地从信号处理器里执行。
//!
//! # 使用方式
//!
//! ```ignore
//! fn main() {
//!     error::run(mainloop);          // 安装 hook + 信号处理器，然后跑业务，最后统一退出
//! }
//!
//! fn mainloop() -> error::Result<()> {
//!     let guard = writer::TerminalGuard::new()
//!         .map_err(|e| error::Error::terminal("初始化终端", e))?;
//!     // ... 业务逻辑，出错就 `?`
//!     Ok(())
//! }
//! ```
//!
//! # 约定
//!
//! 1. [`install`] 必须在 **任何终端模式改动之前** 调用（[`run`] 已经代劳）。
//! 2. 谁把终端切进 raw mode / 备用屏，谁就负责 [`set_terminal_owned`]`(true)`；
//!    临时把终端交给子进程时先 `(false)`，拿回来再 `(true)`。
//! 3. 本模块假定 **panic 是致命的**。若将来把 `panic` 改成 `unwind`，请给每个
//!    `thread::spawn` 的监控线程加 `catch_unwind`，否则监控线程里的 panic 会误触发
//!    终端恢复，而主循环还在跑。

use std::cell::UnsafeCell;
use std::fmt;
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::process;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use libc::c_int;

// ---------------------------------------------------------------------------
// 一、错误类型
// ---------------------------------------------------------------------------

/// 本 crate 统一的 `Result`。
pub type Result<T> = std::result::Result<T, Error>;

/// bazaar 的错误类型。
///
/// 只分四类，因为分类的唯一用途是决定 **退出码** 和 **给用户的提示**，
/// 分得再细也不会有人去 match。
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// 配置问题：文件缺失、TOML 语法错、字段类型不对等。用户自己能修。
    Config(String),
    /// 运行环境缺东西：没有声卡、连不上 DBus、找不到背光设备等。
    Dependency {
        /// 缺的是什么，例如 `"ALSA 混音器"`。
        what: &'static str,
        /// 具体原因。
        detail: String,
    },
    /// 终端控制失败：`enable_raw_mode`、写备用屏、取窗口大小等。
    Terminal {
        /// 正在做什么，例如 `"enable_raw_mode"`。
        action: &'static str,
        /// 底层错误。
        source: io::Error,
    },
    /// 其它 IO 错误。
    Io {
        /// 正在做什么，例如 `"读取配置文件"`。
        context: &'static str,
        /// 底层错误。
        source: io::Error,
    },
    /// 非 IO 来源的杂项错误（第三方库返回的、拼装出来的）。
    Other {
        /// 正在做什么。
        context: &'static str,
        /// 具体原因。
        detail: String,
    },
}

impl Error {
    /// 构造一个配置错误。
    pub fn config(msg: impl Into<String>) -> Self {
        Error::Config(msg.into())
    }

    /// 构造一个"缺依赖"错误。
    ///
    /// 供接下来的 `monitors.rs` 改造使用（ALSA / DBus / 背光都会用到），先备好。
    #[allow(dead_code)]
    pub fn dependency(what: &'static str, detail: impl fmt::Display) -> Self {
        Error::Dependency {
            what,
            detail: detail.to_string(),
        }
    }

    /// 构造一个终端错误。
    pub fn terminal(action: &'static str, source: io::Error) -> Self {
        Error::Terminal { action, source }
    }

    /// 构造一个 IO 错误。
    pub fn io(context: &'static str, source: io::Error) -> Self {
        Error::Io { context, source }
    }

    /// 构造一个杂项错误（非 IO 来源，比如第三方库的 error）。
    ///
    /// 同样是为 `monitors.rs` 改造预留。
    #[allow(dead_code)]
    pub fn other(context: &'static str, detail: impl fmt::Display) -> Self {
        Error::Other {
            context,
            detail: detail.to_string(),
        }
    }

    /// 这个错误应该以什么退出码结束进程。
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Error::Config(_) => ExitCode::Config,
            Error::Dependency { .. } => ExitCode::Dependency,
            Error::Terminal { .. } => ExitCode::Terminal,
            Error::Io { .. } | Error::Other { .. } => ExitCode::Failure,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(msg) => write!(f, "配置错误: {msg}"),
            Error::Dependency { what, detail } => write!(f, "缺少依赖（{what}）: {detail}"),
            Error::Terminal { action, source } => write!(f, "终端操作失败（{action}）: {source}"),
            Error::Io { context, source } => write!(f, "{context} 失败: {source}"),
            Error::Other { context, detail } => write!(f, "{context} 失败: {detail}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Terminal { source, .. } | Error::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    /// 兜底转换。**会在打印时丢掉"在做什么"这个上下文**，所以除非上下文显然，
    /// 优先用 [`Context::context`] 显式标注。
    fn from(source: io::Error) -> Self {
        Error::Io {
            context: "IO 操作",
            source,
        }
    }
}

/// 给 `io::Result` 补上"在做什么"的上下文。
///
/// ```ignore
/// let text = fs::read_to_string(path).context("读取配置文件")?;
/// ```
pub trait Context<T> {
    /// 出错时附上 `context`（一个简短的动宾短语）。
    fn context(self, context: &'static str) -> Result<T>;
}

impl<T> Context<T> for io::Result<T> {
    fn context(self, context: &'static str) -> Result<T> {
        self.map_err(|source| Error::Io { context, source })
    }
}

// ---------------------------------------------------------------------------
// 二、退出码
// ---------------------------------------------------------------------------

/// 进程退出码。数字一旦发布就别改，脚本/状态栏可能在用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    /// 正常退出（用户按了 `q`）。
    Ok = 0,
    /// 未分类的失败。
    Failure = 1,
    /// 配置有问题，用户改配置文件就能好。
    Config = 2,
    /// 终端初始化/控制失败（多半不是 tty）。
    Terminal = 3,
    /// 运行时依赖缺失（声卡、DBus、背光……）。
    Dependency = 4,
    /// 内部错误，代码有 bug。沿用 Rust 的 panic 退出码 101
    /// （注意 `panic = "abort"` 下真实 panic 会以 SIGABRT / 134 结束，
    /// 这个码留给显式调用的场合）。
    #[allow(dead_code)]
    Internal = 101,
}

impl ExitCode {
    /// 转成 `process::exit` 要的数字。
    pub const fn code(self) -> i32 {
        self as i32
    }
}

// ---------------------------------------------------------------------------
// 三、终端状态：任何路径都能还回去
// ---------------------------------------------------------------------------

/// 当前是否由我们持有终端（raw mode + 备用屏）。
static TERMINAL_OWNED: AtomicBool = AtomicBool::new(false);

/// 是否成功保存过进入 raw mode 之前的 termios。
static TERMIOS_SAVED: AtomicBool = AtomicBool::new(false);

/// 进入 raw mode 之前的 termios。
///
/// 只在 [`save_terminal_state`] 里写一次（发生在安装信号处理器之前），
/// 之后所有读取都来自信号处理器/panic hook，因此不存在数据竞争。
/// 用裸 `UnsafeCell` 而不是 `Mutex`：信号处理器里不能加锁。
/// （`std::cell::SyncUnsafeCell` 在本工具链上仍是 unstable，所以自己包一层。）
struct SavedTermios(UnsafeCell<MaybeUninit<libc::termios>>);

// SAFETY: 写只发生一次，且在安装信号处理器之前完成；此后仅做 Copy 读取。
unsafe impl Sync for SavedTermios {}

static SAVED_TERMIOS: SavedTermios = SavedTermios(UnsafeCell::new(MaybeUninit::uninit()));

/// 正在运行的子进程 pid（`<= 0` 表示没有）。退出时会被 SIGTERM 收拾掉。
static CHILD_PID: AtomicI32 = AtomicI32::new(-1);

/// 记下"我们持有终端"。进入 raw mode / 备用屏之后调用。
pub fn set_terminal_owned(owned: bool) {
    TERMINAL_OWNED.store(owned, Ordering::SeqCst);
}

/// 保存当前 termios，作为将来恢复的目标。幂等。
///
/// 必须在 **进入 raw mode 之前** 调用；[`install`] 会做一次，
/// `TerminalGuard::new` 也会在切模式前再保险一次。
pub fn save_terminal_state() {
    if TERMIOS_SAVED.load(Ordering::SeqCst) {
        return;
    }
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: fd 有效时 tcgetattr 只写 t；失败时 t 不被采信。
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut t) } == 0 {
        unsafe { (*SAVED_TERMIOS.0.get()).write(t) };
        TERMIOS_SAVED.store(true, Ordering::SeqCst);
    }
}

/// 登记子进程 pid，退出时若它还在跑就 SIGTERM 掉，避免留下孤儿进程占着终端。
pub fn track_child(pid: i32) {
    if pid > 0 {
        CHILD_PID.store(pid, Ordering::SeqCst);
    }
}

/// 子进程已经回收，别再管它了。
///
/// 必须和 [`track_child`] 配对调用：pid 会被系统复用，
/// 留着过期的 pid 可能在退出时误杀无辜进程。
pub fn untrack_child() {
    CHILD_PID.store(-1, Ordering::SeqCst);
}

/// 幂等地把终端还给用户。
///
/// 可以从普通代码、panic hook、信号处理器里调用：[`TERMINAL_OWNED`] 用
/// `swap` 抢占，只有第一个调用者真正动手，所以重复调用无害。
///
/// 这里刻意 **不用** `crossterm::terminal::disable_raw_mode()`——它内部要拿一把
/// `Mutex`（crossterm-0.27 `terminal/sys/unix.rs:98`）。如果信号正好打断持有该锁的
/// 主线程，从信号处理器里再调它就会自己把自己锁死。裸 `tcsetattr` 没有这个问题。
///
/// 只做三件事，全部不分配、不加锁：
/// 1. `kill` 掉登记在册的子进程；
/// 2. 显示光标 + 退出备用屏（`write(2)` 直写 fd 1，绕过 Rust 的缓冲和锁）；
/// 3. 恢复进入 raw mode 之前的 termios。
fn restore_terminal() {
    kill_tracked_child();

    if !TERMINAL_OWNED.swap(false, Ordering::SeqCst) {
        return; // 终端不在我们手里（例如正交给 btop），别乱动
    }

    // SAFETY: 全是无副作用的 libc 调用，返回值显式忽略。
    unsafe {
        const RESTORE: &[u8] = b"\x1b[?25h\x1b[?1049l"; // 显示光标 + 退出备用屏
        let _ = libc::write(
            libc::STDOUT_FILENO,
            RESTORE.as_ptr().cast(),
            RESTORE.len(),
        );

        if TERMIOS_SAVED.load(Ordering::Relaxed) {
            let saved = (*SAVED_TERMIOS.0.get()).assume_init_ref();
            let _ = libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, saved);
        }
    }
}

/// 正常退出路径上调用（等价于 [`restore_terminal`]，供 `TerminalGuard::drop` 使用）。
pub fn restore_terminal_at_exit() {
    restore_terminal();
}

/// SIGTERM 掉登记的子进程（如果还在）。
fn kill_tracked_child() {
    let pid = CHILD_PID.swap(-1, Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: kill 对任意 pid 都是安全的；进程已退出只会得到 ESRCH。
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

// ---------------------------------------------------------------------------
// 四、三种退出路径
// ---------------------------------------------------------------------------

/// 安装 panic hook 与信号处理器，并保存 termios。
///
/// 必须在任何终端模式改动之前、且只调用一次。
pub fn install() {
    save_terminal_state();
    install_signal_handlers();
    install_panic_hook();
}

/// 统一的程序入口：跑 `f`，按结果决定退出码，然后走 [`exit`]。
///
/// 返回 `!`，所以可以直接写在 `main` 里：
/// `fn main() { error::run(mainloop) }`。
pub fn run(f: impl FnOnce() -> Result<()>) -> ! {
    install();

    let code = match f() {
        Ok(()) => ExitCode::Ok,
        Err(e) => {
            report(&e);
            e.exit_code()
        }
    };

    exit(code)
}

/// 唯一的进程出口：刷缓冲 → 还终端 → 带码退出。
///
/// 注意 `process::exit` 不跑析构函数，所以终端恢复必须在调用它之前显式完成——
/// 这正是本模块存在的意义。
pub fn exit(code: ExitCode) -> ! {
    let _ = io::stdout().flush();
    restore_terminal();
    process::exit(code.code())
}

/// 打印错误链和修复提示。不 panic、不保证顺序，失败就算了。
pub fn report(err: &Error) {
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "bazaar: {err}");

    let mut source = std::error::Error::source(err);
    while let Some(e) = source {
        let _ = writeln!(stderr, "  ↳ 起因: {e}");
        source = e.source();
    }

    match err {
        Error::Config(_) => {
            let _ = writeln!(
                stderr,
                "  提示: 配置文件位于 ~/.config/bazaar/config.toml，修好它或删掉它重新生成默认配置"
            );
        }
        Error::Dependency { what, .. } => {
            let _ = writeln!(stderr, "  提示: 当前环境缺少 {what}，可在配置里去掉对应组件");
        }
        Error::Terminal { .. } => {
            let _ = writeln!(stderr, "  提示: bazaar 需要跑在真实终端里，不支持重定向/管道");
        }
        _ => {}
    }
}

/// panic hook：先还终端，再让默认 hook 打印消息和回溯。
///
/// `panic = "abort"` 下 hook 依然会被调用（abort 发生在 hook 之后），
/// 所以即使进程随后被 abort，用户也能拿回一个可用的终端。
fn install_panic_hook() {
    let previous = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        let _ = writeln!(
            io::stderr(),
            "\nbazaar 内部错误（panic），终端已恢复。请连同下面的信息一起反馈："
        );
        previous(info);
    }));
}

/// 信号处理器：还终端 → 恢复默认处置 → 重新抛出，让退出状态如实反映信号。
///
/// SIGKILL / SIGSTOP 抓不到，SIGSEGV / SIGBUS 故意不抓（栈已经不可信，
/// 在里面跑恢复逻辑只会把现场搞得更乱）。
extern "C" fn on_fatal_signal(sig: c_int) {
    restore_terminal();

    // 处理器执行期间该信号是被屏蔽的（除非装了 SA_NODEFER），此时直接 raise 只会把它
    // 挂成 pending，进程会若无其事地继续跑。所以要先解除屏蔽、恢复默认处置再重新抛出，
    // 让进程真正"死于该信号"：waitpid 能看到 WIFSIGNALED，SIGQUIT 也还能出 core。
    //
    // SAFETY: 以下全是 async-signal-safe 的调用。
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, sig);
        libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());

        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
        libc::_exit(128 + sig); // 兜底：万一上面没能终结进程
    }
}

/// 给致命信号装上处理器。失败就算了——没有它程序照样能跑，
/// 只是被 kill 时终端可能留在 raw mode。
fn install_signal_handlers() {
    // SAFETY: 处理器本身只调用 async-signal-safe 的函数。
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_fatal_signal as *const () as usize;
        sa.sa_flags = 0;
        libc::sigemptyset(&mut sa.sa_mask);

        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGQUIT, libc::SIGHUP] {
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

// ---------------------------------------------------------------------------
// 五、测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 退出码按错误类别分流() {
        assert_eq!(Error::config("x").exit_code(), ExitCode::Config);
        assert_eq!(
            Error::dependency("ALSA", "无声卡").exit_code(),
            ExitCode::Dependency
        );
        assert_eq!(
            Error::terminal("enable_raw_mode", io::Error::other("x")).exit_code(),
            ExitCode::Terminal
        );
        assert_eq!(
            Error::io("读配置", io::Error::other("x")).exit_code(),
            ExitCode::Failure
        );
        assert_eq!(ExitCode::Internal.code(), 101);
    }

    #[test]
    fn context_保留上下文和起因() {
        let e = io::Error::other("permission denied");
        let err = Err::<(), _>(e).context("读取配置文件").unwrap_err();
        assert_eq!(err.to_string(), "读取配置文件 失败: permission denied");
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn 恢复是幂等的() {
        // 没持有终端时不该动任何东西
        set_terminal_owned(false);
        restore_terminal();
        restore_terminal();
        assert!(!TERMINAL_OWNED.load(Ordering::SeqCst));
    }
}
