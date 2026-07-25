use std::io::Write;
use std::fs;
use std::path::PathBuf;

use libc::{syscall, SYS_pidfd_open, pid_t};

use toml;

use crate::{monitors::Monitor, writer::{Block, TerminalGuard}};
mod monitors;

mod writer;
mod commander;
//use rustbus
/*
[liuno@liuno ~]$ sudo busctl monitor org.freedesktop.NetworkManager
[liuno@liuno ~]$ sudo busctl monitor net.connman.iwd
[liuno@liuno ~]$ sudo busctl monitor org.bluez
[liuno@liuno ~]$ sudo busctl monitor org.freedesktop.Notifications

notify-send "通知标题" "通知正文内容"
*/

struct Register {
    fdm:Vec<usize>,
    fds:Vec<libc::pollfd>,
    monitors:Vec<Box<dyn Monitor>>,
    pending_child:Option<PendingChild>,
    commander:commander::Commander,
    writer:writer::Writer,
    sort:Vec<Vec<usize>>,
    selector:(u8,u8),
    flush:bool
}
impl Register {
    fn new(writer:writer::Writer) -> Self {
        Self {
            fdm:Vec::new(),
            fds:vec![libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 }],//基础FD
            monitors:Vec::new(),
            pending_child:None,
            commander:commander::Commander::new(),
            writer,
            sort:Vec::new(),
            selector:(0,0),
            flush:true
        }
    }

    fn init(&mut self,out: &mut impl Write){
        self.sort = self.writer.get_sort();

        let mut i = 0;
        for m in self.monitors.iter_mut() {
            self.writer.update_block(out,i,m.get_data());
            i = i+1;
        }
    }

    fn regist(&mut self,fds: Vec<libc::pollfd>,monitor:Box<dyn Monitor>,block:Block,command:String){
        self.fdm.push(fds.len());
        self.fds.extend(fds);
        self.monitors.push(monitor);
        self.writer.add_block(block);
        self.commander.add_command(command);
    }

    fn run_command(&mut self,t:&TerminalGuard,out: &mut impl Write){
        let selector = self.writer.get_selector() as usize;
        let cmd = self.commander.command(selector);
        
        if let Err(e) = t.leave_terminal() {
            eprintln!("Failed to leave terminal: {}", e);
            return;
        }

        self.fds[0].events = 0;   // 不再监听 stdin

        match std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .spawn()
        {
            Ok(child) => {
                let pid = child.id() as pid_t;
                let pidfd = unsafe { syscall(SYS_pidfd_open, pid, 0) };

                if pidfd < 0 {
                    eprintln!("spawn error: pidfd < 0");
                    let _ = t.enter_terminal();
                    self.fds[0].events = libc::POLLIN;
                    self.flush = true;
                    self.writer.update_all(out);
                }
                
                let pidfd = pidfd as i32;
                self.fds.push(libc::pollfd {
                    fd: pidfd,
                    events: libc::POLLIN,
                    revents: 0,
                });

                self.pending_child = Some(PendingChild { child, pidfd });
                self.flush = false;
            }

             Err(e) => {
                eprintln!("spawn error: {}", e);
                let _ = t.enter_terminal();
                self.fds[0].events = libc::POLLIN;
                self.flush = true;
                self.writer.update_all(out);
            }
        }
    }

    fn selector_up(&mut self,out: &mut impl Write) {
        let (_,mut y) = self.selector;

        if y <= 0 {
            return;
        }
        y -= 1;

        let g = &self.sort[y as usize];
        let l = g.len() - 1;

        self.writer.set_selector(out, g[l] as u16);

        self.selector = (l as u8,y)
    }

    fn selector_down(&mut self,out: &mut impl Write) {
        let (_,mut y) = self.selector;

        if y as usize >= self.sort.len() - 1 {
            return;
        }
        y += 1;

        self.writer.set_selector(out, self.sort[y as usize][0] as u16);
        
        self.selector = (0,y)
    }

    fn selector_left(&mut self,out: &mut impl Write) {
        let (mut x,mut y) = self.selector;

        if x <= 0 {
            if y <= 0 {
                return;
            }
            y -= 1;
            let g = &self.sort[y as usize];
            x = (g.len() - 1) as u8;

            self.writer.set_selector(out, g[x as usize] as u16);

            self.selector = (x,y);

            return;
        }
        
        x -= 1;
        self.writer.set_selector(out, self.sort[y as usize][x as usize] as u16);
        
        self.selector = (x,y)
    }

    fn selector_right(&mut self,out: &mut impl Write) {
        let (mut x,mut y) = self.selector;

        if x as usize >= self.sort[y as usize].len() - 1 {
            if y as usize >= self.sort.len() - 1 {
                return;
            }
            y += 1;
            x = 0;

            self.writer.set_selector(out, self.sort[y as usize][x as usize] as u16);

            self.selector = (x,y);

            return;
        }
        
        x += 1;
        self.writer.set_selector(out, self.sort[y as usize][x as usize] as u16);
        
        self.selector = (x,y)
    }
    
}

struct PendingChild {
    child: std::process::Child,
    pidfd: i32,
}

fn get_config_path() -> Option<PathBuf> {
    unsafe extern "C" {
        fn getpwuid(uid: u32) -> *mut libc::passwd;
        fn getuid() -> u32;
    }

    let mut home: PathBuf = unsafe {

        let uid = getuid();

        let pwd = getpwuid(uid);
        if pwd.is_null() {
            return None;
        }
        let home_cstr = (*pwd).pw_dir;
        if home_cstr.is_null() {
            return None;
        }

        let home = match std::ffi::CStr::from_ptr(home_cstr).to_str() {
            Ok(s) => s,
            Err(_) => return None,
        };
        PathBuf::from(home)
    };
    home.push(".config");
    home.push("bazaar");
    home.push("config.toml");
    
    Some(home)
}

fn load_config(out: &mut impl Write) -> Option<Register>{
    let p = match get_config_path() {
        Some(path) => path,
        None => {
            return None
        }
    };

    if !p.is_file() {
        match create_default_config(&p) {
            Err(_) => return None,
            Ok(_) => {}
        }
    }

    let text = match fs::read_to_string(p) {
        Ok(t) => t,
        Err(_) => {
            return None
        }
    };

    let config: toml::Value = match toml::from_str(&text) {
        Ok(c) => c,
        Err(_) => {
            return None
        }
    };

    let layout = config.get("layout").expect("缺少 [layout] 配置");

    let comp_list = config.get("components")
    .and_then(|v| v.as_array())
    .expect("[[components]] 配置错误");

    let w = writer::Writer::start(out, layout.get("rows").expect("缺少 [layout] 配置").to_string());

    let mut r = Register::new(w);
    for comp in comp_list {
        let (monitor,fds):(Box<dyn Monitor>,Vec<libc::pollfd>)
        = match comp["type"].as_str() {
            Some(s) => {
                match s {
                    "bazaar" => {
                        let (t,fds) = monitors::Bazaar::new();
                        (Box::new(t),fds)
                    }
                    "time" => {
                        let (t, fds) = monitors::Timer::new();
                        (Box::new(t),fds)
                    }
                    "brightness" => {
                        let (t, fds) = monitors::BrightnessMonitor::new();
                        (Box::new(t),fds)
                    }
                    "alsa" => {
                        let (t, fds) = monitors::ALSAMonitor::new();
                        (Box::new(t),fds)
                    }
                    "network" => {
                        let (t, fds) = monitors::NmMonitor::new();
                        (Box::new(t),fds)
                    }
                    "bluetooth" => {
                        continue;
                        let (t, fds) = monitors::BtMonitor::new();
                        (Box::new(t),fds)
                    }
                    "workspace" => {
                        let (t, fds) = monitors::WSMonitor::new();
                        (Box::new(t),fds)
                    }
                    _ => continue,
                }
            }
            None => {
                continue;
            }
        };

        let command = match comp["command"].as_str() {
            Some(c) => c.to_string(),
            None => continue
        };

        let (x,dx) = match comp["x"].as_str() {
            Some(x_dx) => {
                match x_dx.split_once("%") {
                    Some((x,dx)) => {
                        (x.trim().parse().unwrap_or(0),dx.trim().parse::<i16>().unwrap_or(0))
                    }
                    None => (0,0)
                }
            }
            None => {
                (0,0)
            }
        };

        let (y,dy) = match comp["y"].as_str() {
            Some(y_dy) => {
                match y_dy.split_once("%") {
                    Some((y,dy)) => {
                        (y.trim().parse().unwrap_or(0),dy.trim().parse::<i16>().unwrap_or(0))
                    }
                    None => (0,0)
                }
            }
            None => {
                (0,0)
            }
        };

        let l = match comp["length"].as_str() {
            Some(l) => {
                l.trim().parse().unwrap_or(0)
            }
            None => {
                0
            }
        };

        let block = Block::new(x, dx, y, dy, l);

        r.regist(fds, monitor, block, command);

        //let w = match
    }
    

    Some(r)
}

fn create_default_config(path: &PathBuf) -> std::io::Result<()>  {
    if path.exists() {
        if path.is_dir() {
            fs::remove_dir_all(path)?;   // 删除整个目录树
        } else {
            fs::remove_file(path)?;      // 删除文件
        }
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let default_content = 
r#"
[layout]
rows = 2

[[components]]
type = "time"

command = "btop"

x = "100%-23"
y = "0%2"
length = "21"

[[components]]
type = "brightness"

command = "sh command"

x = "0%2"
y = "0%1"
length = "5"

[[components]]
type = "alsa"

command = "alsamixer"

x = "0%8"
y = "0%1"
length = "11"

[[components]]
type = "bluetooth"

command = "bluetui"

x = "0%20"
y = "0%1"
length = "30"

[[components]]
type = "network"

command = "nmtui"

x = "50%10"
y = "0%1"
length = "30"

[[components]]
type = "workspace"

command = "sh command"

x = "0%2"
y = "0%2"
length = "14"

[[components]]
type = "bazaar"

command = "yazi"

x = "50%-3"
y = "0%2"
length = "6"
"#;
    let mut file = std::fs::File::create(path)?;
    file.write_all(default_content.as_bytes())?;

    Ok(())
}

fn main() {
    mainloop();
}

fn mainloop() {
    let terminal_guard = writer::TerminalGuard::new().expect("终端初始化失败");

    let mut out = std::io::stdout().lock();

    // 从配置文件创建注册表
    let mut register = match load_config(&mut out) {
        Some(r ) => r,
        None => {
            panic!()
        }
    };

    register.init(&mut out);

    // 主循环
    loop {
        let ret = unsafe { libc::poll(register.fds.as_mut_ptr(), register.fds.len() as u64, -1) };
        if ret < 0 { 
            if ret == -1  {
                let err = unsafe { *libc::__errno_location() };
                if err == libc::EINTR {
                    continue;
                }
            }
            continue;
        }

        //监控子进程是否关闭
        if let Some(ref mut pending) = register.pending_child {
            if let Some(last_pfd) = register.fds.last() {
                if last_pfd.fd == pending.pidfd && (last_pfd.revents & libc::POLLIN != 0) {
                    let _ = pending.child.try_wait();
                    unsafe { libc::close(pending.pidfd); }

                    register.fds.pop();

                    register.pending_child = None;

                    if let Err(e) = terminal_guard.enter_terminal() {
                        panic!("Failed to re-enter terminal: {}", e);
                    }

                    register.fds[0].events = libc::POLLIN;
                    register.flush = true;
                    register.writer.update_all(&mut out);
                }
            }
        }

        // 按键
        if register.fds[0].revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 8]; 
            let n = unsafe { libc::read(register.fds[0].fd, buf.as_mut_ptr() as _, 3) };
            if n > 0 {
                let n = n as usize;
                if handle_stdin(&buf[..n],&mut register,&mut out,&terminal_guard) {
                    break;
                }
            }
            register.fds[0].revents = 0;
        }

        for i in 1..register.fds.len() {
            if register.fds[i].revents & libc::POLLIN != 0 {
                let mut i_copy = i - 1;
                for (enu,&m) in register.fdm.iter().enumerate() {
                    if i_copy < m {
                        let s = register.monitors[enu].get_data();
                        if register.flush {
                            register.writer.update_block(&mut out, enu, s);
                        }
                        break;
                    }
                    i_copy -= m;
                }
                
            }
            register.fds[i].revents = 0;
        }

        if register.flush {
            register.writer.check_size(&mut out);
            out.flush().unwrap();
        }
    }
}

fn handle_stdin(data:&[u8],r : &mut Register, out: &mut impl Write ,t : &TerminalGuard) -> bool{
    if data.is_empty() { return false; }

    if data[0] == b'q' { return true; }

    if data[0] == b'n' { notepad(); return false; }

    if data == b"\n" || data == b"\r" {
        r.run_command(t, out);
        return false;
    }

    if data.len() >= 3 && data[0] == 0x1b && data[1] == b'[' {
        match data[2] {
            b'A' => r.selector_up(out),
            b'B' => r.selector_down(out),
            b'C' => r.selector_right(out),
            b'D' => r.selector_left(out),
            _ => {}
        }
        return false;
    }

    if data.len() == 1 {
        match data[0] {
            b'h' => r.selector_left(out),
            b'j' => r.selector_down(out),
            b'k' => r.selector_up(out),
            b'l' => r.selector_right(out),
            _ => {}
        }
    }

    false
}

fn notepad(){

}

//消息通知