//! sn420b — set up an SN / Xprinter 420B label printer on Wi-Fi from Linux.
//!
//! The 420B speaks ZPL, so it uses CUPS's built-in "Zebra ZPL" driver over raw
//! port 9100. Wi-Fi is set over USB with the printer's TSPL "WIFI ..." commands;
//! if DHCP moves the printer, "fix" rescans for it and repoints the queue.
//! https://github.com/kenners22/sn420b-linux

use std::ffi::CStr;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Read, Write};
use std::net::{Ipv4Addr, TcpStream, ToSocketAddrs};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{self, Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

const QUEUE: &str = "SN420B";
const PORT: u16 = 9100;
const PPD: &str = "drv:///sample.drv/zebra.ppd";

const USAGE: &str = "\
sn420b — set up an SN / Xprinter 420B label printer on Wi-Fi from Linux.

The 420B speaks ZPL, so it uses CUPS's built-in \"Zebra ZPL\" driver over raw
port 9100. Wi-Fi is set over USB with the printer's TSPL \"WIFI ...\" commands;
if DHCP moves the printer, \"fix\" rescans for it and repoints the queue.
https://github.com/kenners22/sn420b-linux

  sn420b wifi          join the printer to Wi-Fi over USB (prompts for password)
  sn420b usbip         ask the printer (over USB) what Wi-Fi IP it has
  sn420b selftest      print the self-test/config page (over USB)
  sn420b find          scan the LAN for the printer
  sn420b setup [IP]    add/update the CUPS queue \"SN420B\" (asks for sudo)
  sn420b fix           re-find the printer if its IP changed, repoint the queue
  sn420b status        show where it is and whether it's reachable
  sn420b test          print a test label straight to the printer";

macro_rules! say {
    ($($t:tt)*) => { eprintln!($($t)*) };
}

fn die(msg: impl std::fmt::Display) -> ! {
    say!("error: {msg}");
    process::exit(1)
}

/// `$VAR`, treating an empty value like an unset one (bash's `${VAR:-…}`).
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------- config

/// Saved printer location: `IP=…` / `MAC=…` lines, same file the bash version used.
struct Conf {
    path: PathBuf,
    ip: String,
    mac: String,
}

impl Conf {
    fn load() -> Conf {
        let dir = env_nonempty("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env_nonempty("HOME").unwrap_or_default()).join(".config"));
        let path = dir.join("sn420b.conf");
        let (ip, mac) = fs::read_to_string(&path).map(|t| parse_conf(&t)).unwrap_or_default();
        Conf { path, ip, mac }
    }

    fn save(&self) {
        if let Some(dir) = self.path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Err(e) = fs::write(&self.path, format!("IP={}\nMAC={}\n", self.ip, self.mac)) {
            die(format!("can't write {}: {e}", self.path.display()));
        }
    }
}

fn parse_conf(text: &str) -> (String, String) {
    let (mut ip, mut mac) = (String::new(), String::new());
    for line in text.lines() {
        let Some((k, v)) = line.trim().split_once('=') else { continue };
        let v = v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
        match k.trim() {
            "IP" => ip = v,
            "MAC" => mac = v,
            _ => {}
        }
    }
    (ip, mac)
}

// ---------------------------------------------------------------- network

/// Interface of the lowest-metric IPv4 default route in /proc/net/route.
fn parse_default_iface(route: &str) -> Option<String> {
    route
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let flags = u32::from_str_radix(f.get(3)?, 16).ok()?;
            let metric: u32 = f.get(6)?.parse().ok()?;
            (f[1] == "00000000" && flags & 1 != 0).then(|| (metric, f[0].to_string()))
        })
        .min_by_key(|(m, _)| *m)
        .map(|(_, iface)| iface)
}

fn iface_ipv4(name: &str) -> Option<Ipv4Addr> {
    let mut found = None;
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return None;
        }
        let mut p = ifap;
        while !p.is_null() {
            let ifa = &*p;
            if !ifa.ifa_addr.is_null()
                && i32::from((*ifa.ifa_addr).sa_family) == libc::AF_INET
                && CStr::from_ptr(ifa.ifa_name).to_bytes() == name.as_bytes()
            {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                found = Some(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)));
                break;
            }
            p = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    found
}

/// First three octets of this machine's address on the default-route interface.
fn subnet() -> Option<[u8; 3]> {
    let iface = parse_default_iface(&fs::read_to_string("/proc/net/route").ok()?)?;
    let o = iface_ipv4(&iface)?.octets();
    Some([o[0], o[1], o[2]])
}

fn port_open(host: &str) -> bool {
    let Ok(mut addrs) = (host, PORT).to_socket_addrs() else { return false };
    addrs.next().is_some_and(|a| TcpStream::connect_timeout(&a, Duration::from_secs(1)).is_ok())
}

/// MAC for `ip` from the kernel's ARP table (complete entries only).
fn parse_arp(arp: &str, ip: &str) -> Option<String> {
    arp.lines().skip(1).find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        let flags = u32::from_str_radix(f.get(2)?.trim_start_matches("0x"), 16).ok()?;
        (f[0] == ip && flags & 0x2 != 0 && f[3] != "00:00:00:00:00:00").then(|| f[3].to_string())
    })
}

fn mac_of(ip: &str) -> String {
    fs::read_to_string("/proc/net/arp").ok().and_then(|t| parse_arp(&t, ip)).unwrap_or_default()
}

/// List hosts on this /24 with port 9100 open. Each connect attempt makes the
/// kernel resolve the host's MAC, so the ARP table is fresh afterwards.
fn scan() -> Vec<String> {
    let net = subnet().unwrap_or_else(|| die("not connected to a network"));
    let prefix = format!("{}.{}.{}", net[0], net[1], net[2]);
    say!("Scanning {prefix}.0/24 for printers on port {PORT}…");
    let hosts: Vec<String> = (1..=254).map(|i| format!("{prefix}.{i}")).collect();

    let checks: Vec<_> = hosts
        .into_iter()
        .map(|h| thread::spawn(move || port_open(&h).then_some(h)))
        .collect();
    checks.into_iter().filter_map(|t| t.join().ok().flatten()).collect()
}

fn none_if_empty(hits: &[String]) -> String {
    if hits.is_empty() { "none".into() } else { hits.join(" ") }
}

// ---------------------------------------------------------------- CUPS

fn sudo(args: &[&str]) {
    match Command::new("sudo").args(args).status() {
        Ok(s) if s.success() => {}
        Ok(s) => process::exit(s.code().unwrap_or(1)),
        Err(e) => die(format!("couldn't run sudo {}: {e}", args[0])),
    }
}

fn cmd_find() {
    let hits = scan();
    if hits.is_empty() {
        say!("No printer found. Is it on and joined to Wi-Fi (2.4 GHz)?");
        process::exit(1);
    }
    for h in hits {
        println!("{h}  {}", mac_of(&h));
    }
}

fn cmd_setup(conf: &mut Conf, target: Option<String>) {
    let target = match target {
        Some(t) => t,
        None => {
            let mut hits = scan();
            if hits.len() != 1 {
                say!("Found {} printers: {}. Run: sn420b setup <IP>", hits.len(), none_if_empty(&hits));
                process::exit(1);
            }
            hits.remove(0)
        }
    };
    if !port_open(&target) {
        die(format!("{target} isn't answering on port {PORT}"));
    }
    conf.mac = mac_of(&target);
    conf.ip = target;
    conf.save();
    let uri = format!("socket://{}:{PORT}", conf.ip);
    say!("Setting up queue {QUEUE} → {uri} (sudo needed)");
    sudo(&[
        "lpadmin", "-p", QUEUE, "-E", "-v", &uri, "-m", PPD,
        "-D", "SN 420B label printer", "-L", "Wi-Fi",
        "-o", "PageSize=w288h432", "-o", "Darkness-default=15", "-o", "printer-error-policy=retry-job",
    ]);
    say!("Done. Print with: lp -d {QUEUE} file.pdf   (4x6 labels)");
}

fn cmd_fix(conf: &mut Conf) {
    if conf.ip.is_empty() {
        die("no saved printer — run: sn420b setup");
    }
    if port_open(&conf.ip) {
        say!("Printer still at {} — nothing to fix.", conf.ip);
        return;
    }
    // Find it by its open print port (MAC is unreliable if it roams onto an
    // extender that rewrites client MACs).
    let mut hits = scan();
    if hits.len() != 1 {
        die(format!("found {} printers ({}) — run: sn420b setup <IP>", hits.len(), none_if_empty(&hits)));
    }
    conf.ip = hits.remove(0);
    conf.mac = mac_of(&conf.ip);
    conf.save();
    say!("Printer moved to {}; updating queue (sudo needed)", conf.ip);
    sudo(&["lpadmin", "-p", QUEUE, "-v", &format!("socket://{}:{PORT}", conf.ip)]);
    sudo(&["cupsenable", QUEUE]);
}

fn cmd_status(conf: &Conf) {
    if conf.ip.is_empty() {
        say!("Not set up yet — run: sn420b setup");
        process::exit(1);
    }
    println!("Saved:  {} ({})", conf.ip, conf.mac);
    if port_open(&conf.ip) {
        println!("Printer: reachable");
    } else {
        println!("Printer: NOT reachable (try: sn420b fix)");
    }
    let _ = io::stdout().flush();
    let ok = Command::new("lpstat")
        .args(["-p", QUEUE, "-v", QUEUE])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        println!("CUPS queue {QUEUE}: not installed");
    }
}

fn local_time() -> String {
    // localtime() (not _r) reads TZ / /etc/localtime; fine, we're single-threaded here.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let p = libc::localtime(&t);
        if p.is_null() {
            return String::new();
        }
        let tm = *p;
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min
        )
    }
}

fn test_label(target: &str, when: &str) -> String {
    format!(
        "^XA^PW812^LL1218^FO60,80^A0N,70,70^FDSN 420B^FS^FO60,180^A0N,40,40^FDWi-Fi printing OK^FS\
         ^FO60,240^A0N,30,30^FD{target}  {when}^FS^FO60,320^BY3^BCN,120,Y,N,N^FDSN420B^FS^XZ"
    )
}

fn cmd_test(conf: &Conf, target: Option<String>) {
    let target = target.unwrap_or_else(|| conf.ip.clone());
    if target.is_empty() {
        die("no printer — run: sn420b setup, or: sn420b test <IP>");
    }
    let addr = (target.as_str(), PORT)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .unwrap_or_else(|| die(format!("can't resolve {target}")));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .unwrap_or_else(|e| die(format!("can't connect to {target}:{PORT}: {e}")));
    s.write_all(test_label(&target, &local_time()).as_bytes())
        .unwrap_or_else(|e| die(format!("sending to {target}: {e}")));
    say!("Test label sent to {target}");
}

// ---------------------------------------------------------------- USB

fn usb_dev() -> String {
    env_nonempty("SN420B_DEV").unwrap_or_else(|| "/dev/usb/lp0".into())
}

/// Send a TSPL command over USB and return the reply up to an OUT marker.
/// Commands are the ones the vendor's "SN printer tool" uses.
fn usb_cmd(cmd: &str, marker: &str, wait: Duration) -> String {
    let dev = usb_dev();
    let mut f = match OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(&dev) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            die(format!("{dev} not found — is the printer plugged in by USB? (set SN420B_DEV to override)"))
        }
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            let user = env_nonempty("USER").unwrap_or_else(|| "$USER".into());
            die(format!("no access to {dev} — run: sudo setfacl -m u:{user}:rw {dev}  (or install the udev rule)"))
        }
        Err(e) => die(format!("can't open {dev}: {e}")),
    };
    let end = Instant::now() + wait;

    let mut out = cmd.as_bytes();
    while !out.is_empty() {
        match f.write(out) {
            Ok(n) => out = &out[n..],
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < end => {
                thread::sleep(Duration::from_millis(50))
            }
            Err(e) => die(format!("writing to {dev}: {e}")),
        }
    }

    let marker_b = marker.as_bytes();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !contains(&buf, marker_b) && Instant::now() < end {
        let mut pfd = libc::pollfd { fd: std::os::fd::AsRawFd::as_raw_fd(&f), events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut pfd, 1, 500) };
        match f.read(&mut chunk) {
            Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
            // Nothing yet (or a transient USB error): don't spin.
            _ => thread::sleep(Duration::from_millis(50)),
        }
    }
    String::from_utf8_lossy(&buf).replace(marker, "").trim().to_string()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// First dotted quad (digits.digits.digits.digits) in the text.
fn extract_ip(text: &str) -> Option<String> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.')).find_map(|run| {
        let parts: Vec<&str> = run.split('.').collect();
        parts.windows(4).find(|w| w.iter().all(|p| !p.is_empty())).map(|w| w.join("."))
    })
}

fn usb_ip() -> Option<String> {
    extract_ip(&usb_cmd("WIFI GETIP\r\nOUT \"SNIP\"\r\n", "SNIP", Duration::from_secs(10)))
}

static SAVED_TERMIOS: OnceLock<libc::termios> = OnceLock::new();

extern "C" fn restore_tty_and_exit(_: libc::c_int) {
    if let Some(t) = SAVED_TERMIOS.get() {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
    }
    unsafe { libc::_exit(130) };
}

/// Read one line like bash `read -r`: newline dropped, outer spaces/tabs trimmed.
fn read_line(prompt: &str, hidden: bool) -> String {
    eprint!("{prompt}");
    let _ = io::stderr().flush();
    let tty = unsafe { libc::isatty(0) } == 1;
    let mut saved: Option<libc::termios> = None;
    if hidden && tty {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) == 0 {
                let _ = SAVED_TERMIOS.set(t);
                libc::signal(libc::SIGINT, restore_tty_and_exit as extern "C" fn(libc::c_int) as libc::sighandler_t);
                let mut quiet = t;
                quiet.c_lflag &= !libc::ECHO;
                libc::tcsetattr(0, libc::TCSANOW, &quiet);
                saved = Some(t);
            }
        }
    }
    let mut line = String::new();
    let _ = io::stdin().lock().read_line(&mut line);
    if let Some(t) = saved {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
    }
    if hidden {
        say!();
    }
    line.trim_end_matches(['\n', '\r']).trim_matches([' ', '\t']).to_string()
}

fn cmd_wifi() {
    let ssid = read_line("Wi-Fi network name (SSID): ", false);
    if ssid.is_empty() {
        die("network name can't be empty");
    }
    let pwd = read_line(&format!("Password for \"{ssid}\" (hidden): "), true);
    if pwd.is_empty() {
        die("password can't be empty");
    }
    if ssid.contains('"') || pwd.contains('"') {
        die("names/passwords containing \" aren't supported by the printer command");
    }
    say!("Sending Wi-Fi settings to the printer over USB…");
    usb_cmd(
        &format!("WIFI CONNECTION \"{ssid}\",\"{pwd}\"\r\nWIFI RESET\r\nWIFI GETIP\r\nOUT \"SNSET\"\r\n"),
        "SNSET",
        Duration::from_secs(40),
    );
    drop(pwd);

    say!("Waiting for it to join (up to ~60s)…");
    let mut ip = None;
    for _ in 0..12 {
        ip = usb_ip().filter(|i| i != "0.0.0.0");
        if ip.is_some() {
            break;
        }
        thread::sleep(Duration::from_secs(5));
    }
    match ip {
        Some(ip) => {
            port_open(&ip); // fills the ARP entry so we can show the MAC
            let mac = mac_of(&ip);
            let mac_note = if mac.is_empty() { String::new() } else { format!(" (MAC {})", mac.to_uppercase()) };
            say!("Printer joined \"{ssid}\" at {ip}{mac_note}");
            say!("Next: reserve {ip} for that MAC in your router's DHCP settings, then: sn420b setup {ip}");
        }
        None => {
            say!("Printer didn't get an IP. Re-check the password (it's case-sensitive), use your main");
            say!("network rather than a guest one, and try WPA2 if your router is WPA3-only.");
            process::exit(1);
        }
    }
}

// ---------------------------------------------------------------- main

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).filter(|a| !a.is_empty()).cloned();
    let mut conf = Conf::load();
    match args.first().map(String::as_str).unwrap_or("help") {
        "find" => cmd_find(),
        "setup" => cmd_setup(&mut conf, arg(1)),
        "fix" => cmd_fix(&mut conf),
        "status" => cmd_status(&conf),
        "test" => cmd_test(&conf, arg(1)),
        "wifi" => cmd_wifi(),
        "selftest" => {
            usb_cmd("SELFTEST\r\nOUT \"SNST\"\r\n", "SNST", Duration::from_secs(10));
            say!("Self-test page sent");
        }
        "usbip" => match usb_ip() {
            Some(ip) => println!("{ip}"),
            None => process::exit(1),
        },
        _ => println!("{USAGE}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conf_roundtrip_and_quotes() {
        assert_eq!(parse_conf("IP=10.0.0.42\nMAC=dc:0d:30:12:34:56\n"), ("10.0.0.42".into(), "dc:0d:30:12:34:56".into()));
        assert_eq!(parse_conf("# x\nIP=\"10.0.0.5\"\nMAC=''\n"), ("10.0.0.5".into(), "".into()));
    }

    #[test]
    fn default_route_lowest_metric() {
        let route = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
            wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
            eth0\t00000000\t0100A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
            wlan0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(parse_default_iface(route).as_deref(), Some("eth0"));
        assert_eq!(parse_default_iface("Iface\tDestination\n"), None);
    }

    #[test]
    fn arp_lookup_skips_incomplete() {
        let arp = "IP address       HW type     Flags       HW address            Mask     Device\n\
            10.0.0.50      0x1         0x0         00:00:00:00:00:00     *        wlan0\n\
            10.0.0.42    0x1         0x2         dc:0d:30:12:34:56     *        wlan0\n";
        assert_eq!(parse_arp(arp, "10.0.0.42").as_deref(), Some("dc:0d:30:12:34:56"));
        assert_eq!(parse_arp(arp, "10.0.0.50"), None);
        assert_eq!(parse_arp(arp, "10.0.0.4"), None);
    }

    #[test]
    fn ip_from_printer_reply() {
        assert_eq!(extract_ip("+OK=10.0.0.42\r\n").as_deref(), Some("10.0.0.42"));
        assert_eq!(extract_ip("WIFI GETIP\r\n+OK=0.0.0.0").as_deref(), Some("0.0.0.0"));
        assert_eq!(extract_ip("v1.037 +OK_CONNECTED_STATUS"), None);
        assert_eq!(extract_ip("x.1.2.3.4.5").as_deref(), Some("1.2.3.4"));
    }

    #[test]
    fn label_matches_bash_version() {
        assert_eq!(
            test_label("10.0.0.42", "2026-10-05 09:30"),
            "^XA^PW812^LL1218^FO60,80^A0N,70,70^FDSN 420B^FS^FO60,180^A0N,40,40^FDWi-Fi printing OK^FS\
             ^FO60,240^A0N,30,30^FD10.0.0.42  2026-10-05 09:30^FS^FO60,320^BY3^BCN,120,Y,N,N^FDSN420B^FS^XZ"
        );
    }
}
