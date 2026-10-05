//! wolrs — Wake-on-LAN CLI + Web 管理介面
//!
//! 功能:
//!   - 命名儲存機器:name / MAC / 可選 IP(JSON 持久化)
//!   - Web UI + JSON API:新增、喚醒、刪除、ping 狀態
//!   - CLI:`wolrs <mac>` 喚醒、`wolrs <ip>` ping
//!
//! 資料檔:env WOLRS_DATA,或預設 $HOME/.wolrs/machines.json

use std::env;
use std::io::{Read, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct Machine {
    name: String,
    mac: String,
    ip: Option<String>,
    /// 固定發射/探測用網卡(雙網卡環境避免走錯卡)
    #[serde(default)]
    iface: Option<String>,
}

static STATE: OnceLock<Mutex<Vec<Machine>>> = OnceLock::new();

fn data_path() -> std::path::PathBuf {
    if let Ok(p) = env::var("WOLRS_DATA") {
        return std::path::PathBuf::from(p);
    }
    let home = env::var("HOME").unwrap_or_else(|_| "/root".into());
    std::path::PathBuf::from(home).join(".wolrs").join("machines.json")
}

fn load() -> Vec<Machine> {
    std::fs::read_to_string(data_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn with_state<F: FnOnce(&mut Vec<Machine>)>(f: F) {
    let s = STATE.get_or_init(|| Mutex::new(load()));
    let mut g = s.lock().unwrap();
    f(&mut g);
    save(&g);
}

fn save(machines: &[Machine]) {
    let path = data_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = {
        let mut t = path.clone().into_os_string();
        t.push(".tmp");
        std::path::PathBuf::from(t)
    };
    let json = serde_json::to_string_pretty(machines).unwrap();
    if std::fs::write(&tmp, &json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

// ---------- 網卡掃描(預設優先有線) ----------

struct Iface {
    name: String,
    addr: [u8; 4],
    netmask: [u8; 4],
    bcast: Option<[u8; 4]>,
    wired: bool,
}

const SIOCGIFADDR: u32 = 0x8915;
const SIOCGIFBRDADDR: u32 = 0x8919;
const SIOCGIFNETMASK: u32 = 0x891B;

fn iface_is_up(name: &str) -> bool {
    std::fs::read(format!("/sys/class/net/{}/operstate", name))
        .map(|s| String::from_utf8_lossy(&s).trim() == "up")
        .unwrap_or(false)
}

fn iface_wired(name: &str) -> bool {
    // 有 /sys/class/net/<if>/wireless 視為無線卡
    !std::path::Path::new(&format!("/sys/class/net/{}/wireless", name)).exists()
}

struct Ifr {
    name: [libc::c_char; 16],
    sa: libc::sockaddr_in,
}

fn ioctl_ifa<S: AsRef<str> + std::fmt::Debug>(fd: libc::c_int, cmd: u32, ifname: S) -> Option<libc::sockaddr_in> {
    let n = S::as_ref(&ifname);
    if n.is_empty() {
        return None;
    }
    unsafe {
        let mut ifr = std::mem::zeroed::<Ifr>();
        let c = std::ffi::CString::new(n).ok();
        let bytes = c.as_ref().map(|c| c.as_bytes()).unwrap_or(&[] as &[u8]);
        let len = std::cmp::min(bytes.len(), 15);
        ifr.name[..len].copy_from_slice(&bytes[..len].iter().map(|b| *b as libc::c_char).collect::<Vec<_>>()[..]);
        ifr.name[len] = 0;
        ifr.sa.sin_family = libc::AF_INET as u16;
        let ret = libc::ioctl(fd, cmd as _, &ifr);
        if ret < 0 {
            None
        } else {
            Some(ifr.sa)
        }
    }
}

fn octets(v: u32) -> [u8; 4] {
    // in_addr.s_addr 的記憶體位元組序 = IP 位元組;LE 機上從 u32 讀:低 8 位 = 第一個 octet
    v.to_be_bytes()
}

fn scan_ifaces() -> Vec<Iface> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/sys/class/net") {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name == "lo" || !iface_is_up(&name) {
                continue;
            }
            unsafe {
                let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
                if fd < 0 {
                    continue;
                }
                if let Some(addr) = ioctl_ifa(fd, SIOCGIFADDR, name.clone()) {
                    if let Some(mask) = ioctl_ifa(fd, SIOCGIFNETMASK, name.clone()) {
                        let bcast = ioctl_ifa(fd, SIOCGIFBRDADDR, &name).map(|b| octets(b.sin_addr.s_addr));
                        let wired = iface_wired(&name);
                        out.push(Iface {
                            name,
                            addr: octets(addr.sin_addr.s_addr),
                            netmask: octets(mask.sin_addr.s_addr),
                            bcast,
                            wired,
                        });
                    }
                }
                libc::close(fd);
            }
        }
    }
    out
}

fn in_net(ifc: &Iface, ip: [u8; 4]) -> bool {
    (0..4).all(|i| (ifc.addr[i] ^ ip[i]) & ifc.netmask[i] == 0)
}

/// 依需求排序:1) 指定 iface 置頂  2) 目標 IP 所在子網匹配的介面  3) 有線卡  4) 其餘介面
/// 回傳所有 up 的 IPv4 介面(雙網卡環境讓呼叫端可逐卡 fallback)
fn pick_ifaces(target_ip: Option<[u8; 4]>, force: Option<&str>) -> Vec<Iface> {
    let mut ifs = scan_ifaces();
    if let Some(f) = force {
        ifs.sort_by_key(|i| if i.name == f { 0 } else { 1 });
        return ifs;
    }
    ifs.sort_by(|a, b| {
        let ka = if target_ip.map_or(false, |ip| in_net(a, ip)) { 0 } else { 1 };
        let kb = if target_ip.map_or(false, |ip| in_net(b, ip)) { 0 } else { 1 };
        ka.cmp(&kb)
            // 同子網匹配狀態時,有線卡在前(wired=true 於 false)
            .then(b.wired.cmp(&a.wired))
            .then(a.name.len().cmp(&b.name.len()))
    });
    ifs
}

fn broadcast_of(ifc: &Iface) -> [u8; 4] {
    if let Some(b) = ifc.bcast {
        return b;
    }
    let mut out = [0u8; 4];
    for i in 0..4 {
        out[i] = ifc.addr[i] & ifc.netmask[i] | !ifc.netmask[i];
    }
    out
}

// ---------- WoL ----------

/// 指定 iface(query/CLI/機器設定/WOLRS_IFACE):只透過該網卡 broadcast,不存在/未 up 時直接報錯,
/// 避免雙網卡環境 magic packet 走錯卡;未指定則依「目標子網匹配 > 有線卡 > 其他」
/// 對所有 up 的介面發送,再以 255.255.255.255 保底(kernel 選路)。
fn send_wol(mac: [u8; 6]) -> std::io::Result<Vec<SocketAddr>> { send_wol_opt(mac, None, None, None) }

/// 依名稱找網卡(僅 up 的介面)
fn iface_by_name(name: &str) -> Option<Iface> {
    scan_ifaces().into_iter().find(|i| i.name == name)
}

/// 依候選網卡順序逐一 ping(雙網卡環境:第一張卡失敗時自動換下一張),
/// 任一成功即回傳 (true, 經由網卡);全失敗再試不 bind 的 default route。
/// 回傳 (是否 online, 經由網卡或 None)。
fn ping_via_any(target_ip: [u8; 4], force: Option<&str>) -> (bool, Option<String>) {
    let p = std::net::Ipv4Addr::from(target_ip);
    let cands = pick_ifaces(Some(target_ip), force);
    for (i, c) in cands.iter().enumerate() {
        // 最優先的候選給完整超時,其餘 fallback 縮短,避免總等待過長
        let timeout: u64 = if i == 0 { 1000 } else { 500 };
        if icmp_ping(std::net::Ipv4Addr::from(c.addr), timeout).0 {
            return (true, Some(c.name.clone()));
        }
    }
    if cands.is_empty() {
        // 沒有任何 up 的 IPv4 介面:純 kernel 選路
        let ok = icmp_ping(std::net::Ipv4Addr::from(p), 1000).0;
        return (ok, if ok { Some("default-route".into()) } else { None });
    }
    if icmp_ping(std::net::Ipv4Addr::from(p), 500).0 {
        return (true, Some("default-route".into()));
    }
    (false, None)
}

fn send_wol_opt(mac: [u8; 6], target_ip: Option<[u8; 4]>, force_iface: Option<&str>, prefer_iface: Option<&str>) -> std::io::Result<Vec<SocketAddr>> {
    let packet = [vec![0xFFu8; 6], mac.to_vec().repeat(16)].concat();
    // 優先序:query/CLI 明確指定 > 機器設定 > env WOLRS_IFACE
    let force = force_iface
        .map(|s| s.to_string())
        .or_else(|| prefer_iface.map(|s| s.to_string()))
        .or_else(|| env::var("WOLRS_IFACE").ok())
        .filter(|s| !s.is_empty());
    let ifcs: Vec<Iface> = match force.as_deref() {
        Some(f) => match iface_by_name(f) {
            Some(i) => vec![i],
            None => return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("iface {} not found or not up", f),
            )),
        },
        None => pick_ifaces(target_ip, None),
    };
    let mut sent = Vec::new();
    for ifc in &ifcs {
        let bind = std::net::Ipv4Addr::from(ifc.addr);
        let to = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::from(broadcast_of(ifc))), 9u16);
        let sock = std::net::UdpSocket::bind((bind, 0))?;
        sock.set_broadcast(true)?;
        sock.send_to(&packet, to)?;
        sent.push(to);
    }
    // 保底:global broadcast(由 kernel 選路,雙網卡環境可能走錯卡,故放最後,且仅在未明確指定網卡時才發)
    if force.is_none() && !sent.is_empty() {
        let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
        sock.set_broadcast(true)?;
        let to = SocketAddr::from(([255, 255, 255, 255], 9u16));
        sock.send_to(&packet, to)?;
        sent.push(to);
    }
    Ok(sent)
}

fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        if p.len() != 2 {
            return None;
        }
        out[i] = u8::from_str_radix(p, 16).ok()?;
    }
    Some(out)
}

fn norm_mac(s: &str) -> Option<String> {
    parse_mac(s).map(|m| m.map(|b| format!("{:02x}", b)).to_vec().join(":"))
}

// ---------- Ping (ICMP, unprivileged via SOCK_DGRAM, fallback SOCK_RAW) ----------

/// ICMP echo;bind_ip 為 Some 時把 socket 綁定到該本地來源 IP,
/// 讓 kernel 走對應網卡(雙網卡環境避免 ping 從錯誤介面出去)。
///
/// 封包採標準 ICMP 布局: [0]=type 8(request) [1]=code [2..4]=checksum
/// [4..6]=id [6..8]=seq [8..]=payload。
/// Linux SOCK_DGRAM ICMP 的三個坑:
///   1. sendto 的 sin_port 必須等於 ICMP type(8),否則 EINVAL
///   2. checksum 無效時 sendto 直接 EINVAL
///   3. kernel 會用自己的 socket id 做 demux,回覆的 id 不再是我們送的值,
///      所以 dgram 端只檢查 type==0 即可(kernel 已確保包是給我們的)
/// SOCK_RAW 端 kernel 不攔截,檢查 type==0 && id 匹配(標準偏移 [off+4..off+6])。
// ---------- ICMP ping(仿 iputils: dgram 不 bind、sin_port=0、poll 到 deadline) ----------

fn icmp_ping(ip: std::net::Ipv4Addr, timeout_ms: u64) -> (bool, u64) {
    let oct = ip.octets();
    let t0 = std::time::Instant::now();
    let timeout = std::time::Duration::from_millis(timeout_ms.max(100));
    let dbg = std::env::var_os("WOLRS_DEBUG").is_some();

    // ICMP echo request, len 16: type=8 code=0, checksum, id, seq=1, "wolrswol"
    let mut pkt: [u8; 16] = [0u8; 16];
    pkt[0] = 8;
    let id: u16 = std::process::id() as u16;
    pkt[4] = (id & 0xff) as u8;
    pkt[5] = (id >> 8) as u8;
    pkt[6] = 1u8;  pkt[7] = 0u8;
    pkt[8..16].copy_from_slice(b"wolrswol");
    // one's complement checksum over whole packet
    {
        let mut sum: u32 = 0;
        for w in pkt.chunks_exact(2) { sum += u16::from_be_bytes([w[0], w[1]]) as u32; }
        while sum > 0xffff { sum = (sum >> 16) + (sum & 0xffff); }
        let csum = (sum & 0xffff) as u16;
        pkt[2] = (!csum & 0xff) as u8;
        pkt[3] = (!csum >> 8) as u8;
    }

    // dest sockaddr, sin_port=0 (matches iputils; s_addr network order)
    let mut a: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    a.sin_family = libc::AF_INET as u16;
    a.sin_port = 0u16;
    a.sin_addr.s_addr = (oct[0] as u32) | (oct[1] as u32) << 8 | (oct[2] as u32) << 16 | (oct[3] as u32) << 24;

    let mut rb: [u8; 128] = [0u8; 128];

    // ---- try 1: SOCK_DGRAM, no bind (like real ping) — kernel matches reply per-socket.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_ICMP as i32) };
    if fd >= 0 {
        let sn = unsafe {
            libc::sendto(fd, pkt.as_ptr() as *const libc::c_void, pkt.len(), 0,
                &a as *const _ as *const libc::sockaddr, 16)
        };
        if sn >= 0 {
            if dbg { eprintln!("[dbg] dgram sent"); }
            loop {
                if t0.elapsed() >= timeout { break; }
                let mut pv: [libc::pollfd; 1] = unsafe { std::mem::zeroed() };
                pv[0].fd = fd;
                pv[0].events = libc::POLLIN as i16;
                let pr = unsafe { libc::poll(pv.as_mut_ptr(), 1, 250) };
                if pr < 0 { break; }
                if pr == 0 { continue; } // poll timeout: keep waiting until deadline
                if (pv[0].revents & (libc::POLLIN as i16)) == 0 { break; }
                let n = unsafe { libc::recv(fd, rb.as_mut_ptr() as *mut libc::c_void, rb.len(), 0) };
                if n < 2 { continue; }
                let n = n as usize;
                let mut off = 0usize;
                while off < n {
                    if rb[off] == 0 {            // echo reply (dgram: kernel-managed id)
                        let ms = t0.elapsed().as_millis() as u64;
                        if dbg { eprintln!("[dbg] dgram reply ms={}", ms); }
                        unsafe { libc::close(fd); }
                        return (true, ms);
                    }
                    off += 1;
                }
            }
        } else if dbg {
            eprintln!("[dbg] dgram sendto: {}", std::io::Error::last_os_error());
        }
        unsafe { libc::close(fd); }
    } else if dbg {
        eprintln!("[dbg] dgram socket: {}", std::io::Error::last_os_error());
    }

    // ---- try 2: SOCK_RAW (bind + id/seq match)
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_RAW, libc::IPPROTO_ICMP as i32) };
    if fd >= 0 {
        let mut any: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        any.sin_family = libc::AF_INET as u16;
        any.sin_port = 0u16;
        any.sin_addr.s_addr = 0u32;
        let _ = unsafe { libc::bind(fd, &any as *const _ as *const libc::sockaddr, 16) };
        let sn = unsafe {
            libc::sendto(fd, pkt.as_ptr() as *const libc::c_void, pkt.len(), 0,
                &a as *const _ as *const libc::sockaddr, 16)
        };
        if sn >= 0 {
            if dbg { eprintln!("[dbg] raw sent"); }
            loop {
                if t0.elapsed() >= timeout { break; }
                let mut pv: [libc::pollfd; 1] = unsafe { std::mem::zeroed() };
                pv[0].fd = fd;
                pv[0].events = libc::POLLIN as i16;
                let pr = unsafe { libc::poll(pv.as_mut_ptr(), 1, 250) };
                if pr < 0 { break; }
                if pr == 0 { continue; }
                if (pv[0].revents & (libc::POLLIN as i16)) == 0 { break; }
                let n = unsafe { libc::recv(fd, rb.as_mut_ptr() as *mut libc::c_void, rb.len(), 0) };
                if n < 8 { continue; }
                let n = n as usize;
                let mut off = 0usize;
                while off + 8 <= n {
                    if rb[off] == 0
                        && rb[off + 4] == (id & 0xff) as u8
                        && rb[off + 5] == (id >> 8) as u8
                        && rb[off + 6] == 1 {
                        let ms = t0.elapsed().as_millis() as u64;
                        if dbg { eprintln!("[dbg] raw reply ms={}", ms); }
                        unsafe { libc::close(fd); }
                        return (true, ms);
                    }
                    off += 1;
                }
            }
        } else if dbg {
            eprintln!("[dbg] raw sendto: {}", std::io::Error::last_os_error());
        }
        unsafe { libc::close(fd); }
    } else if dbg {
        eprintln!("[dbg] raw socket: {}", std::io::Error::last_os_error());
    }

    (false, t0.elapsed().as_millis() as u64)
}

type Resp = (u16, String);
fn get_machines() -> Resp {
    let g = STATE.get_or_init(|| Mutex::new(std::vec::Vec::new())).lock().unwrap();
    (
        200,
        serde_json::json!(&*g).to_string(),
    )
}

fn upsert_machine(body: &str, query: &str) -> Resp {
    #[derive(Deserialize)]
    struct In {
        name: String,
        mac: String,
        #[serde(default)]
        ip: Option<String>,
        #[serde(default)]
        iface: Option<String>,
    }
    let in_data: In = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) =>match serde_json::from_str(query) {
            Ok(v) => v,
            Err(_) => return (400, r#"{"error":"bad JSON, expect {\"name\",\"mac\",\"ip?\"}"}"#.into()),
        },
    };

    let name = in_data.name.trim().to_string();
    let mac = in_data.mac.trim().to_lowercase();
    if name.is_empty() {
        return (400, r#"{"error":"name required"}"#.into());
    }
    if parse_mac(&mac).is_none() {
        return (400, r#"{"error":"bad mac"}"#.into());
    }
    if let Some(ip) = &in_data.ip {
        if !ip.parse::<std::net::Ipv4Addr>().is_ok() {
            return (400, r#"{"error":"ip must be IPv4"}"#.into());
        }
    }

    let machine = Machine {
        name,
        mac: norm_mac(&mac).unwrap(),
        ip: in_data.ip,
        iface: in_data.iface,
    };
    // 名稱唯一性規範化:同 name 視為 upsert
    with_state(|v| {
        if let Some(p) = v.iter_mut().find(|m| m.name.eq_ignore_ascii_case(&machine.name)) {
            *p = machine.clone();
        } else {
            v.push(machine.clone());
        }
    });
    (201, format!(r#"{{"ok":true,"machine":{}}}"#, serde_json::json!(machine)))
}

fn delete_machine(query: &str) -> Resp {
    let name = query_param(query, "name").unwrap_or_default();
    if name.is_empty() {
        return (400, r#"{"error":"name required"}"#.into());
    }
    let removed = machine_find(&name);
    if removed.is_none() {
        return (404, r#"{"ok":false,"error":"not found"}"#.into());
    }
    with_state(|v| v.retain(|m| !m.name.eq_ignore_ascii_case(&name)));
    (200, format!(r#"{{"ok":true,"removed":{}}}"#, serde_json::json!(&removed.unwrap())))
}

fn machine_find(name: &str) -> Option<Machine> {
    STATE
        .get_or_init(|| Mutex::new(std::vec::Vec::new()))
        .lock()
        .unwrap()
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case(name))
        .cloned()
}

fn wake_machine(query: &str) -> Resp {
    let name = query_param(query, "name");
    let mac_q = query_param(query, "mac");

    let machine: Option<Machine> = match (&name, &mac_q) {
        (_, Some(m)) if m.chars().count() == 17 => {
            // 直接以 mac 喚醒
            let m = m.to_lowercase();
            if parse_mac(&m).is_none() {
                return (400, r#"{"error":"bad mac"}"#.into());
            }
            Some(Machine { name: m.clone(), mac: norm_mac(&m).unwrap(), ip: None, iface: None })
        }
        _ => {
            let n = name.unwrap_or_default();
            if n.is_empty() {
                return (400, r#"{"error":"name or mac required"}"#.into());
            }
            machine_find(&n)
        }
    };

    let m = match machine {
        Some(m) => m,
        None => return (404, r#"{"error":"not found"}"#.into()),
    };
    let macb = parse_mac(&m.mac).unwrap();
    let target_ip = m.ip.as_ref().and_then(|s| s.parse::<std::net::Ipv4Addr>().ok()).map(|ip| ip.octets());
    let iface_q = query_param(query, "iface");
    if let Err(e) = send_wol_opt(macb, target_ip, iface_q.as_deref(), m.iface.as_deref()) {
        return (500, format!(r#"{{"error":"send failed: {:?}"}}"#, e));
    }
    (200, format!(r#"{{"ok":true,"woken":{}}}"#, serde_json::json!(m)))
}

fn status_machine(query: &str) -> Resp {
    let name_q = query_param(query, "name");
    let machines = STATE
        .get_or_init(|| Mutex::new(std::vec::Vec::new()))
        .lock()
        .unwrap()
        .clone();
    let targets: Vec<Machine> = match &name_q {
        Some(n) => machines.into_iter().filter(|m| m.name.eq_ignore_ascii_case(n)).collect(),
        None => machines,
    };
    if name_q.is_some() && targets.is_empty() {
        return (404, r#"{"error":"not found"}"#.into());
    }
    let iface_q = query_param(query, "iface");
    let mut out = Vec::new();
    // 並行 ping:每台 ≤1s,避免 N 台串聯 1s×N
    std::thread::scope(|s| {
        let hs: Vec<_> = targets
            .iter()
            .map(|m| {
                let ip = m.ip.as_ref().and_then(|s2| s2.parse::<std::net::Ipv4Addr>().ok());
                let force = iface_q.as_deref().or_else(|| m.iface.as_deref());
                s.spawn(move || ping_machine(m.name.clone(), ip.map(|p| p.octets()), force))
            })
            .collect();
        for h in hs {
            out.push(h.join().unwrap());
        }
    });
    (200, serde_json::to_string(&out).unwrap())
}

/// 對單台機器做狀態探測(雙網卡:逐卡 fallback,
/// 優先序 query iface > 機器設定 iface > 目標子網匹配 > 有線卡 > 其他)
fn ping_machine(name: String, target_ip: Option<[u8; 4]>, force: Option<&str>) -> serde_json::Value {
    match target_ip {
        Some(oct) => {
            let (ok, via) = ping_via_any(oct, force);
            serde_json::json!({ "name": name, "ip": std::net::Ipv4Addr::from(oct).to_string(), "online": ok, "via": via })
        }
        None => serde_json::json!({ "name": name, "ip": null, "online": null, "via": null }),
    }
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .find_map(|kv| kv.split_once('=')?.0.eq(key).then(|| {
            kv.split_once('=').map(|(_, v)| url_decode(v)).unwrap_or_default()
        }))
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let h = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|t| u8::from_str_radix(t, 16).ok());
                if let Some(b) = h {
                    out.push(b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn index_html() -> &'static str {
    r#"<!doctype html>
<html lang="zh-Hant"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>woL-RS 控制台</title>
<style>
body{font-family:system-ui,sans-serif;margin:2rem;max-width:720px;background:#111;color:#eee}
h1{font-size:1.4rem}
table{width:100%;border-collapse:collapse;margin-top:1rem}
th,td{border-bottom:1px solid #333;padding:.5rem .3rem;text-align:left}
.dot{display:inline-block;width:.9rem;height:.9rem;border-radius:50%;margin-right:.4rem;vertical-align:middle}
.dot.on{background:#3c3}.dot.off{background:#c33}.dot.unk{background:#888}
button{background:#265;color:#fff;border:0;padding:.35rem .7rem;border-radius:6px;cursor:pointer;font-size:.9rem}
button.del{background:#633}
input{background:#222;color:#eee;border:1px solid #444;padding:.4rem;border-radius:6px;margin:.15rem 0}
form.add{background:#1a1a1a;padding:1rem;border-radius:10px;margin-top:1.2rem}
form.add label{display:block;font-size:.85rem;color:#bbb;margin:.4rem 0 .1rem}
.small{color:#888;font-size:.8rem;margin-top:1rem}
</style></head><body>
<h1>⚡ 喚醒控制台</h1>
<p class="small">WoL 透過 broadcast 送出,狀態以 ICMP ping 判定(需填 IP)。雙網卡環境可為每部裝置或整體設定 IFACE,避免從錯誤網卡發出。</p>
<table id="list"><thead><tr><th>Name</th><th>MAC</th><th>IP</th><th>Status</th><th></th></tr></thead>
<tbody></tbody></table>
<form class="add" id="addForm">
<label>Name <input name="name" required placeholder="e.g. office-pc"></label>
<label>MAC <input name="mac" required placeholder="aa:bb:cc:dd:ee:ff" pattern="^([0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2}$"></label>
<label>IP (選填,用於狀態檢查) <input name="ip" placeholder="192.168.1.50"></label>
<label>IFACE (選填,雙網卡環境指定網卡) <input name="iface" placeholder="enp6s0"></label>
<button type="submit">新增 / 更新</button>
</form>
<script>
async function load(){
  const r=await fetch('/api/machines');const ms=await r.json();
  let sr=null; try{ sr=await (await fetch('/api/status')).json(); }catch(e){}
  const tbody=document.querySelector('#list tbody');tbody.innerHTML='';
  for(const m of ms){
    const st=(sr&&sr.find(s=>s.name===m.name))||{};
    const dot=st.online==null?'unk':(st.online?'on':'off');
    const tr=document.createElement('tr');
    tr.innerHTML=`<td>${m.name}</td><td><code>${m.mac}</code></td><td>${m.ip||'—'}</td>
    <td><span class="dot ${dot}"></span>${st.online==null?'未知':(st.online?'線上':'離線')}${st.via?' <span class="small">(via '+st.via+')</span>':''}</td>
    <td><button onclick="wake('${m.name}')">喚醒</button>
    <button class="del" onclick="del('${m.name}')">刪</button></td>`;
    tbody.appendChild(tr);
  }
  if(!ms.length) tbody.innerHTML='<tr><td colspan="5" style="color:#666">尚無機器,請下方新增</td></tr>';
}
async function wake(name){
  const r=await fetch('/api/wake?name='+encodeURIComponent(name),{method:'POST'});
  const j=await r.json(); if(!r.ok) alert('Fail: '+(j.error||r.status)); else alert('已傳送喚醒 '+name);
}
async function del(name){
  if(!confirm('刪除 '+name+' ?')) return;
  await fetch('/api/machines?name='+encodeURIComponent(name),{method:'DELETE'});
  load();
}
document.getElementById('addForm').addEventListener('submit',async e=>{
  e.preventDefault();
  const f=new FormData(e.target);
  const body={name:f.get('name').trim(),mac:f.get('mac').trim(),ip:f.get('ip')?f.get('ip').trim():null,iface:f.get('iface')?f.get('iface').trim():null};
  const r=await fetch('/api/machines',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(body)});
  const j=await r.json(); if(!r.ok) alert('Fail: '+(j.error||r.status));
  e.target.reset(); load();
});
load(); setInterval(load,5000);
</script></body></html>"#
}

fn handle(client: &mut std::net::TcpStream) {
    let mut buf = [0u8; 16384];
    let n = client.read(&mut buf).unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]);
    let mut lines = req.split("\r\n\r\n");
    let head = lines.next().unwrap_or("");
    let first_line = head.lines().next().unwrap_or("");
    let mut it = first_line.split_whitespace();
    let method = it.next().unwrap_or("");
    let target = it.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let body = lines.next().unwrap_or("").trim();

    let resp = match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => (200, index_html().to_string()),
        ("GET", "/api/machines") => get_machines(),
        ("GET", "/api/status") => status_machine(query),
        ("GET", "/api/wake") => wake_machine(query),
        ("POST", "/api/machines") => upsert_machine(body, query),
        ("POST", "/api/wake") => wake_machine(query),
        ("DELETE", "/api/machines") => delete_machine(query),
        ("GET", "/healthz") => (200, "ok".into()),
        _ => (404, "not found".into()),
    };

    let (status, body) = resp;
    let reason = match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let is_html = body.starts_with("<!doctype");
    let ctype = if is_html { "text/html; charset=utf-8" } else { "application/json" };
    let r = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{}",
        status, reason, ctype, body.len(), body
    );
    let _ = client.write_all(r.as_bytes());
}

fn main() {
    let mut args_iter = env::args().skip(1);
    let first = args_iter.next();
    let second = args_iter.next(); // 可選:指定發射網卡,如 eth0

    // CLI 模式
    if let Some(arg) = first {
        if let Some(mac) = parse_mac(arg.as_str()) {
            let r = match second.as_deref() {
                Some(i) if !i.is_empty() => send_wol_opt(mac, None, Some(i), None),
                _ => send_wol(mac),
            };
            for to in r.expect("wol send") {
                println!("-> {}", to);
            }
            return;
        }
        if let Ok(ip) = arg.parse::<std::net::Ipv4Addr>() {
            // 第二引數(選填):明確指定探測發射網卡,如 `wolrs 10.0.2.15 enp6s0`
            // 未指定時雙網卡環境會逐卡 fallback(子網匹配 > 有線 > 其他)
            let r = match second.as_deref().filter(|s| !s.is_empty()) {
                Some(f) => match iface_by_name(f) {
                    Some(i) => icmp_ping(std::net::Ipv4Addr::from(i.addr), 1000).0,
                    None => {
                        println!("iface {} not found or not up", f);
                        std::process::exit(2);
                    }
                },
                None => ping_via_any(ip.octets(), None).0,
            };
            println!("{}", if r { "online" } else { "offline" });
            std::process::exit(if r { 0 } else { 1 });
        }
    }

    // 初始化並載入
    with_state(|_v| {});

    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8787);
    let listener = std::net::TcpListener::bind(("0.0.0.0", port)).expect("bind 0.0.0.0:port");
    println!("wolrs web+api on 0.0.0.0:{}  data={}", port, data_path().display());
    for stream in listener.incoming() {
        if let Ok(mut s) = stream {
            std::thread::spawn(move || handle(&mut s));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn valid_mac() {
        let m = parse_mac("00:11:22:33:44:55").unwrap();
        assert_eq!(m, [0, 17, 34, 51, 68, 85]);
        assert_eq!(norm_mac("AB:CD:EF:01:23:45").unwrap(), "ab:cd:ef:01:23:45");
    }
    #[test]
    fn invalid_mac() {
        assert!(parse_mac("1:2:3:4:5:6").is_none());
        assert!(parse_mac("AA:BB:CC:DD:EE").is_none());
        assert!(parse_mac("").is_none());
    }
    #[test]
    fn bad_mac_rejected() {
        assert!(parse_mac("GG:BB:CC:DD:EE:FF").is_none());
    }
}
