use libc::{inotify_add_watch, inotify_init1, pollfd, read, timerfd_create, timerfd_settime};
use libc::{IN_MODIFY, IN_NONBLOCK, CLOCK_MONOTONIC, TFD_NONBLOCK, itimerspec};
use std::time::{SystemTime, UNIX_EPOCH};

use std::collections::HashMap;

use alsa::mixer::{Mixer, SelemId, SelemChannelId};
use alsa::poll::Descriptors;

use zbus::blocking::Connection;
use zbus::blocking::proxy::Proxy;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

// 图标常量
struct Icons;
impl Icons {
    const BRIGHT: &'static str = "󰃠";
    const VOL: &'static str = "󰕾";
    const MIC: &'static str = "";
    const BT: &'static str = "󰂯";
    const WIFI: &'static str = "󰖩";
    const WS: &'static str = "󰙅";
    const TIME: &'static str = "󰃰";
}
/*
// 子进程守卫：退出时自动杀死子进程，防止泄漏
struct ProcessGuard(std::process::Child);
impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
*/
pub trait Monitor:{
    fn new() -> (Self,Vec<pollfd>)
    where
        Self: Sized;
    fn get_data(&mut self) -> String{
        "none".to_string()
    }
}


pub struct Bazaar;
impl Monitor for Bazaar {
    fn new() -> (Self,Vec<pollfd>){
        (Self,vec![pollfd { fd: -1, events: 0, revents: 0 }])
    }

    fn get_data(&mut self) -> String {
        "bazaar".to_string()
    }
}

pub struct Timer{
    fd:i32,
}
impl Monitor for Timer {
    fn new() -> (Self,Vec<pollfd>){
        let fd = create_next_second_timer();
        sleep_to_next_second_timer(fd);
        let fds = vec![pollfd { fd: fd, events: libc::POLLIN, revents: 0 }];
        (Self{ fd },fds)
    }
    fn get_data(&mut self) -> String {
        let mut buf = [0u8; 32];
        let time_str = format_time(&mut buf);
        sleep_to_next_second_timer(self.fd);
        format!("{} {}", Icons::TIME, time_str)
    }
}

// 秒级定时器
fn create_next_second_timer() -> i32 {
    let fd = unsafe { timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK) };
    assert!(fd >= 0, "timerfd 创建失败");
    fd
}

fn sleep_to_next_second_timer(fd: i32) {
    let mut buf = [0u8; 8];
    unsafe { read(fd, buf.as_mut_ptr() as _, 8) };

    let mut now = unsafe { std::mem::zeroed::<libc::timespec>() };
    unsafe { libc::clock_gettime(CLOCK_MONOTONIC, &mut now) };

    let sleep_ns = 1_000_000_000 - now.tv_nsec as u64;

    let mut timer_spec: itimerspec = unsafe { std::mem::zeroed() };
    timer_spec.it_value.tv_nsec = sleep_ns as i64;

    unsafe { timerfd_settime(fd, 0, &timer_spec, std::ptr::null_mut()) };
}

// 栈上格式化时间（零堆分配）
fn format_time(buf: &mut [u8; 32]) -> &str {
    let now = SystemTime::now();
    let local_time = now.duration_since(UNIX_EPOCH).unwrap();
    let ts = local_time.as_secs() as libc::time_t;

    unsafe {
        let mut tm = std::mem::zeroed::<libc::tm>();
        libc::localtime_r(&ts, &mut tm);

        let len = libc::strftime(
            buf.as_mut_ptr() as *mut i8,
            buf.len(),
            b"%Y-%m-%d %H:%M:%S\0".as_ptr() as *const i8,
            &tm,
        );

        std::str::from_utf8_unchecked(&buf[..len])
    }
}

pub struct BrightnessMonitor {
    path:Option<String>,
    max:u8,
}
impl Monitor for BrightnessMonitor {
    fn new() -> (Self,Vec<pollfd>){
        let path = find_brightness_path();
        let max = get_max_brightness(&path);
        let fd = create_brightness_inotify_fd(&path);
        let m = Self{
            path:path,
            max:max,
        };
        let fds = vec![pollfd { fd: fd, events: libc::POLLIN, revents: 0 }];
        (m,fds)
    }

    fn get_data(&mut self) -> String{
        let b = get_brightness(&self.path);
        format!("{} {}%", Icons::BRIGHT, b as u16 * 100 / self.max as u16)
    }
}

fn find_brightness_path() -> Option<String> {
    let backlight_dir = "/sys/class/backlight/";
    for entry in std::fs::read_dir(backlight_dir).ok()? {
        let entry = entry.ok()?;
        let path = entry.path().join("brightness");
        if path.exists() {
            return path.to_str().map(String::from);
        }
    }
    None
}

fn get_max_brightness(p:&Option<String>) -> u8 {
    let path = match p {
        Some(p) => p.replace("brightness", "max_brightness"),
        None => return 100,
    };
    std::fs::read_to_string(path).ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(100)
}

fn get_brightness(p:&Option<String>) -> u8 {
    let path = match p {
        Some(p) => p,
        None => return 0,
    };
    std::fs::read_to_string(path).ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

// 亮度监听inotify
fn create_brightness_inotify_fd(p:&Option<String>) -> i32 {
    let brightness_path = match p {
        Some(p) => p,
        None => return -1,
    };

    let c_path = format!("{}\0", brightness_path);
    let fd = unsafe { inotify_init1(IN_NONBLOCK) };
    if fd < 0 { return -1; }

    unsafe { inotify_add_watch(fd, c_path.as_ptr() as *const i8, IN_MODIFY) };
    fd
}

pub struct ALSAMonitor{
    mixer: Mixer,
}
impl Monitor for ALSAMonitor {
    fn new() -> (Self,Vec<pollfd>){
        // ALSA混音器
        let alsa_mixer = Mixer::new("default", false).unwrap();
        
        // 正确获取ALSA的poll描述符
        let mut alsa_fds: Vec<pollfd> = vec![pollfd { fd: -1, events: 0, revents: 0 }; alsa_mixer.count()];
        alsa_mixer.fill(&mut alsa_fds).unwrap();
        (Self{ mixer: alsa_mixer }, alsa_fds)
    }

    fn get_data(&mut self) -> String {

        let spk_selem = self.mixer.find_selem(&SelemId::new("Master", 0)).unwrap();
        let mic_selem = self.mixer.find_selem(&SelemId::new("Capture", 0)).unwrap();
        
        let _ = self.mixer.handle_events();
        let (vmin, vmax) = spk_selem.get_playback_volume_range();
        let vol = ((spk_selem.get_playback_volume(SelemChannelId::FrontLeft).unwrap_or(vmin) - vmin) * 100 / (vmax - vmin)) as u8;
                
        let (mmin, mmax) = mic_selem.get_capture_volume_range();
        let mic = ((mic_selem.get_capture_volume(SelemChannelId::mono()).unwrap_or(mmin) - mmin) * 100 / (mmax - mmin)) as u8;

        format!("{} {}% {} {}%", Icons::VOL, vol, Icons::MIC, mic)
    }
}
/*
// WiFi监听
pub struct NmMonitor {
    reader: BufReader<ChildStdout>,
    _guard: ProcessGuard,
}

impl Monitor for NmMonitor {
    fn new() -> (Self,Vec<pollfd>) {
        let mut child = Command::new("nmcli")
            .env("LC_ALL", "C")
            .args(["-f", "STATE,CONNECTION", "device", "monitor"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let stdout = child.stdout.take().unwrap();
        let fd = stdout.as_raw_fd();
        let reader = BufReader::new(stdout);
        let _guard = ProcessGuard(child);

        (Self { reader, _guard }, vec![pollfd { fd, events: libc::POLLIN, revents: 0 }])
    }

    fn get_data(&mut self) -> String {
        let mut line = String::new();
        if !self.reader.buffer().is_empty(){
            match self.reader.read_line(&mut line) {
                Err(_) => {}
                Ok(_) =>{}
            }
        }
        format!("{} {}", Icons::WIFI,get_current_wifi())
    }
}

// 获取WiFi名称
fn get_current_wifi() -> String {
    run_cmd("nmcli", &["connection", "show", "--active"], 100)
        .and_then(|s| s.lines().find(|l| l.contains("wifi"))
            .and_then(|l| l.split_whitespace().next())
            .map(|x| x.to_string()))
        .unwrap_or_else(|| "none".into())
}

// 蓝牙监听
pub struct BtMonitor {
    reader: BufReader<ChildStdout>,
    _guard: ProcessGuard,
}

impl Monitor for BtMonitor {
    fn new() -> (Self,Vec<pollfd>) {
        let mut child = Command::new("bluetoothctl")
            .env("LC_ALL", "C")
            .arg("--monitor")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let stdout = child.stdout.take().unwrap();
        let fd = stdout.as_raw_fd();
        let reader = BufReader::new(stdout);
        let _guard = ProcessGuard(child);

        (Self { reader, _guard }, vec![pollfd { fd, events: libc::POLLIN, revents: 0 }])
    }
    fn get_data(&mut self) -> String {
        let mut line = String::new();
        if !self.reader.buffer().is_empty(){
            match self.reader.read_line(&mut line) {
                Err(_) => {}
                Ok(_) =>{}
            }
        }
        format!("{} {}", Icons::BT,get_current_bt())
    }
}

// 获取蓝牙设备
fn get_current_bt() -> String {
    let output = Some("");//run_cmd("bluetoothctl", &["devices", "Connected"], 100);
    let lines = output.as_ref().map(|s| s.lines().collect::<Vec<_>>());

    match lines {
        Some(lines) => {
            let mut names = String::new();
            for line in lines {
                let next = line.split_once(" ").map(|(_, n)| n).unwrap_or_default();
                let name = next.split_once(" ").map(|(_, n)| n).unwrap_or_default();
                names.push_str(name.trim());
            }
            if names.is_empty() { "none".to_string() } else { names }
        }
        None => "none".to_string()
    }
}

// 执行命令（带超时）
fn run_cmd(cmd: &str, args: &[&str], timeout_ms: u64) -> Option<String> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn().ok()?;

    let mut stdout = child.stdout.take()?;
    let timeout = Duration::from_millis(timeout_ms);
    let start = SystemTime::now();

    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let mut buf = Vec::new();
                stdout.read_to_end(&mut buf).ok()?;
                return Some(String::from_utf8_lossy(&buf).to_string());
            }
            Ok(None) => {
                if start.elapsed().unwrap() > timeout {
                    let _ = child.kill();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
}*/

use std::os::unix::io::AsRawFd;
use std::thread;
use nix::unistd::{pipe2, write, read as nix_read};
use nix::fcntl::OFlag;
use zbus::blocking::MessageIterator;
use zbus::MatchRule;
use zbus::message::Type;

pub struct NmMonitor {
    conn: Connection,
    cache: String,
    pipe_rd: std::fs::File,
}

impl Monitor for NmMonitor {
    fn new() -> (Self, Vec<pollfd>) {
        let (rd_owned, wr_owned) = pipe2(OFlag::O_NONBLOCK).unwrap();
        let pipe_rd = std::fs::File::from(rd_owned);
        let pipe_wr = std::fs::File::from(wr_owned);

        let conn = Connection::system().expect("无法连接系统 DBus");

        let conn_clone = conn.clone();

        thread::spawn(move || {
            let rule = MatchRule::builder()
                .msg_type(Type::Signal)
                .interface("org.freedesktop.DBus.Properties").unwrap()
                .member("PropertiesChanged").unwrap()
                .path_namespace("/org/freedesktop/NetworkManager").unwrap()
                .build();

            let msg_iter = MessageIterator::for_match_rule(rule, &conn_clone, Some(1)).unwrap();
            
            for msg in msg_iter {
                let msg = match msg {
                    Ok(m) => m,
                    Err(_) => break,
                };
                let body = msg.body();
                let parse_res = body.deserialize::<(
                    &str,
                    std::collections::HashMap<String, OwnedValue>,
                    Vec<String>
                )>();
                if let Ok((iface, _, _)) = parse_res {
                    if iface == "org.freedesktop.NetworkManager.Device" {
                        let _ = write(&pipe_wr, &[b'x']);
                    }
                }
            }
        });
        let mut monitor = Self {
            conn,
            cache: String::new(),
            pipe_rd,
        };
        monitor.refresh_cache();
        
        let pfd = pollfd {
            fd: monitor.pipe_rd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        (monitor, vec![pfd]) 
    }

    fn get_data(&mut self) -> String {
        let mut buf = [0u8;32];
        let _ = nix_read(&self.pipe_rd, &mut buf);
        self.refresh_cache();
        format!("{} {}", Icons::WIFI, self.cache)
    }
}

impl NmMonitor {
    fn refresh_cache(&mut self) {
        self.cache = get_current_wifi(&self.conn);
    }
}

fn get_current_wifi(conn: &Connection) -> String {
    let Ok(nm_proxy) = Proxy::new(
        conn,
        "org.freedesktop.NetworkManager",
        "/org/freedesktop/NetworkManager",
        "org.freedesktop.NetworkManager",
    ) else {
        return "none".to_string();
    };

    let Ok(devices) = nm_proxy.call::<_, _, Vec<OwnedObjectPath>>("GetDevices", &()) else {
        return "none".to_string();
    };

    for device_path in devices {
        let Ok(dev_proxy) = Proxy::new(
            conn,
            "org.freedesktop.NetworkManager",
            device_path.as_str(),
            "org.freedesktop.NetworkManager.Device",
        ) else {
            continue;
        };

        // NM_DEVICE_TYPE_WIFI = 2
        let Ok(device_type) = dev_proxy.get_property::<u32>("DeviceType") else {
            continue;
        };
        if device_type != 2 {
            continue;
        }

        let Ok(active_conn) = dev_proxy.get_property::<OwnedObjectPath>("ActiveConnection") else {
            continue;
        };
        if active_conn.as_str() == "/" {
            continue;
        }

        let Ok(conn_proxy) = Proxy::new(
            conn,
            "org.freedesktop.NetworkManager",
            active_conn.as_str(),
            "org.freedesktop.NetworkManager.Connection.Active",
        ) else {
            continue;
        };
        let Ok(id) = conn_proxy.get_property::<String>("Id") else {
            continue;
        };

        return id;
    }

    "none".to_string()
}
pub struct BtMonitor {
    conn: Connection,
    cache: String,
    pipe_rd: std::fs::File,
}

impl Monitor for BtMonitor {
    fn new() -> (Self, Vec<pollfd>) {
        let (rd_owned, wr_owned) = pipe2(OFlag::O_NONBLOCK).unwrap();
        let pipe_rd = std::fs::File::from(rd_owned);
        let pipe_wr = std::fs::File::from(wr_owned);

        let conn = Connection::system().expect("无法连接系统 DBus");
        let conn_clone = conn.clone();

        thread::spawn(move || {
            let rule = MatchRule::builder()
                .msg_type(Type::Signal) // msg_type 直接返回builder，无Result
                .interface("org.freedesktop.DBus.Properties").unwrap()
                .member("PropertiesChanged").unwrap()
                .path_namespace("/org/bluez").unwrap()
                .build(); // build() 返回 MatchRule，不是Result
            
            let msg_iter = MessageIterator::for_match_rule(rule, &conn_clone, Some(1)).unwrap();

            for msg in msg_iter {
                //eprintln!("y");
                let msg = match msg {
                    Ok(m) => m,
                    Err(_) => break,
                };
                let body = msg.body();
                let parse_res = body.deserialize::<(
                    &str,
                    std::collections::HashMap<String, OwnedValue>,
                    Vec<String>
                )>();
                if let Ok((iface, _, _)) = parse_res {
                    if iface == "org.bluez.Device1" {
                        let _ = write(&pipe_wr, &[b'x']);
                    }
                }
            }
        });

        let mut monitor = Self {
            conn,
            cache: String::new(),
            pipe_rd,
        };
        monitor.refresh_cache();
        let pfd = pollfd {
            fd: monitor.pipe_rd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        (monitor, vec![pfd])
    }

    fn get_data(&mut self) -> String {
        let mut buf = [0u8; 32];
        let _ = nix_read(&self.pipe_rd, &mut buf);
        self.cache = get_current_bt(&self.conn);
        format!("{} {}", Icons::BT, self.cache)
    }
}

impl BtMonitor {
    fn refresh_cache(&mut self) {
        self.cache = get_current_bt(&self.conn);
    }
}

fn get_current_bt(conn: &Connection) -> String {
    let Ok(obj_mgr) = Proxy::new(
        conn,
        "org.bluez",
        "/",
        "org.freedesktop.DBus.ObjectManager",
    ) else {
        return "conn_err1".to_string();
    };

    let Ok(objects) = obj_mgr.call::<_, _, HashMap<
        OwnedObjectPath,
        HashMap<String, HashMap<String, OwnedValue>>,
    >>("GetManagedObjects", &()) else {
        return "conn_err2".to_string();
    };

    let mut names = String::new();

    for (_, ifaces) in objects {
        let Some(device_props) = ifaces.get("org.bluez.Device1") else {
            continue;
        };

        let Some(connected) = device_props
            .get("Connected")
            .and_then(|v| v.downcast_ref::<bool>().ok())
        else {
            continue;
        };

        if connected {
            let name = device_props
                .get("Name")
                .and_then(|v| v.downcast_ref::<String>().ok())
                .clone() // 将 &String 克隆为拥有所有权的 String
                .unwrap_or_else(|| "unknown".to_string());

            if !names.is_empty() {
                names.push(' ');
            }
            names.push_str(&name);
        }
    }

    if names.is_empty() {
        "none".to_string()
    } else {
        names
    }
}

pub struct WSMonitor;
impl Monitor for WSMonitor {
    fn new() -> (Self,Vec<pollfd>){
        (Self,vec![pollfd { fd: -1, events: 0, revents: 0 }])
    }
    fn get_data(&mut self) -> String {
        format!("{} workspace:{}", Icons::WS,0)
    }
}

//use libc::{fcntl, F_SETFL, O_NONBLOCK};
//use std::path::PathBuf;
//use std::os::unix::net::UnixStream;
//use std::env;
//use std::io::{BufRead, BufReader, Write, stdout,Read,Result,ErrorKind};

// Hyprland Socket路径
/*fn get_hypr_socket_path() -> PathBuf {
    let xdg = env::var("XDG_RUNTIME_DIR").unwrap_or_default();
    let instance = env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap_or_default();
    let mut path = PathBuf::from(xdg);
    path.push("hypr");
    path.push(instance);
    path.push(".socket2.sock");
    path
}

// 获取当前工作区
fn get_current_ws() -> u8 {
    let output = Command::new("hyprctl").arg("activeworkspace").output();
    match output {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            stdout.split_whitespace().nth(2).unwrap_or("1").parse().unwrap_or(1)
        }
        Err(_) => 1,
    }
}

fn read_line(socket: &mut UnixStream) -> Result<String> {
    let mut line = String::new();
    let mut byte = [0u8; 1]; // 每次只读1字节，彻底禁止预读

    loop {
        match socket.read(&mut byte) {
            // 读到1字节
            Ok(n) => {
                if n == 0 {
                    break;
                }
                if byte[0] == b'\n' { // 读到换行符，一行结束
                    break;
                }
                line.push(byte[0] as char);
            }
            // 遇到 WouldBlock，立即返回（不阻塞）
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            // 其他错误
            Err(e) => return Err(e),
        }
    }

    Ok(line)
}
*/
    // Hyprland Socket
    //let mut hypr_stream = UnixStream::connect(get_hypr_socket_path()).unwrap();
    //let hypr_fd = hypr_stream.as_raw_fd();
    //hypr_stream.set_nonblocking(true).expect("设置非阻塞失败");

        // 工作区变化
        /*if fds[5].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            // Hyprland重启，重新连接
            if let Ok(stream) = UnixStream::connect(get_hypr_socket_path()) {
                let fd = stream.as_raw_fd();
                hypr_stream = stream;
                fds[5].fd = fd;
            }
            fds[5].revents = 0;
            continue;
        }
        if fds[5].revents & libc::POLLIN != 0 {

            match read_line(&mut hypr_stream) {
                Err(_)=>{}
                Ok(line)=>{
                    if let Some((event, data)) = line.split_once(">>") {
                        if event == "workspace" {
                            cache.ws = data.trim().parse().unwrap_or(1);
                            cache.update_ws(&mut out);
                         }
                    }
                }
            }
            
            fds[5].revents = 0;
        }*/
