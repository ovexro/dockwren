//! `sudo dockwren setup-ups`: the one command that takes a UPS plugged into the server to a working
//! NUT setup. It installs NUT (Debian/Ubuntu apt), finds the UPS on USB with `nut-scanner`, writes
//! a standalone NUT configuration with a random internal password, fixes the USB permissions and
//! starts the services; NUT's `upsmon` then shuts the server down cleanly when the battery runs
//! low. An existing NUT configuration is never touched. Run again at any time: it is idempotent.

use std::io::Read;
use std::path::Path;
use std::process::Command;

const NUT_DIR: &str = "/etc/nut";
const NAME: &str = "ups";

/// What `nut-scanner -U` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub driver: String,
    pub port: String,
    pub vendorid: String,
    pub productid: String,
}

fn plain(s: &str, extra: &[char]) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
}

fn hex4(s: &str) -> bool {
    s.len() == 4 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// First `[section]` of `nut-scanner` output with a usable driver and USB ids.
pub fn parse_scanner(text: &str) -> Option<Device> {
    let (mut driver, mut port, mut vendorid, mut productid) =
        (String::new(), String::new(), String::new(), String::new());
    let mut seen = false;
    for l in text.lines() {
        let l = l.trim();
        if l.starts_with('[') {
            if seen {
                break;
            }
            seen = true;
            continue;
        }
        let Some((k, v)) = l.split_once('=') else {
            continue;
        };
        let v = v.trim().trim_matches('"').to_string();
        match k.trim() {
            "driver" => driver = v,
            "port" => port = v,
            "vendorid" => vendorid = v,
            "productid" => productid = v,
            _ => {}
        }
    }
    if plain(&driver, &['-', '_'])
        && plain(&port, &['-', '_', '/', '.'])
        && hex4(&vendorid)
        && hex4(&productid)
    {
        Some(Device {
            driver,
            port,
            vendorid: vendorid.to_lowercase(),
            productid: productid.to_lowercase(),
        })
    } else {
        None
    }
}

pub fn ups_conf(d: &Device) -> String {
    format!(
        "# Written by `dockwren setup-ups`.\nmaxretry = 3\n\n[{NAME}]\n    driver = {}\n    port = {}\n    vendorid = {}\n    productid = {}\n    desc = \"UPS (set up by Dockwren)\"\n",
        d.driver, d.port, d.vendorid, d.productid
    )
}

pub fn upsd_users(password: &str) -> String {
    format!(
        "# Written by `dockwren setup-ups`.\n[upsmon]\n    password = {password}\n    upsmon primary\n"
    )
}

/// `upsmon.conf` with our `MONITOR` line in place of any other.
pub fn upsmon_conf(existing: &str, password: &str) -> String {
    let mut out: String = existing
        .lines()
        .filter(|l| !l.trim_start().starts_with("MONITOR "))
        .map(|l| format!("{l}\n"))
        .collect();
    out.push_str(&format!(
        "MONITOR {NAME}@localhost 1 upsmon {password} primary\n"
    ));
    out
}

/// `nut.conf` with `MODE=standalone`.
pub fn nut_conf(existing: &str) -> String {
    let mut out: String = existing
        .lines()
        .filter(|l| !l.trim_start().starts_with("MODE="))
        .map(|l| format!("{l}\n"))
        .collect();
    out.push_str("MODE=standalone\n");
    out
}

/// True when `ups.conf` already defines a UPS (a `[name]` line that is not a comment).
pub fn has_ups_section(ups_conf: &str) -> bool {
    ups_conf.lines().any(|l| l.trim_start().starts_with('['))
}

fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let o = Command::new(cmd)
        .args(args)
        .env("DEBIAN_FRONTEND", "noninteractive")
        .output()
        .map_err(|e| format!("cannot run {cmd}: {e}"))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    } else {
        Err(format!(
            "{cmd} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&o.stderr).trim()
        ))
    }
}

fn find(names: &[&str]) -> Option<String> {
    for dir in ["/usr/sbin", "/usr/bin", "/usr/local/sbin", "/usr/local/bin"] {
        for n in names {
            let p = format!("{dir}/{n}");
            if Path::new(&p).exists() {
                return Some(p);
            }
        }
    }
    None
}

fn random_hex() -> Result<String, String> {
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .map_err(|e| format!("cannot read /dev/urandom: {e}"))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Write a file readable by root and the `nut` group only (it may hold the internal password).
fn write_private(name: &str, body: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let p = format!("{NUT_DIR}/{name}");
    std::fs::write(&p, body).map_err(|e| format!("cannot write {p}: {e}"))?;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o640))
        .map_err(|e| e.to_string())?;
    run("chown", &["root:nut", &p])?;
    Ok(())
}

fn read_or_empty(name: &str) -> String {
    std::fs::read_to_string(format!("{NUT_DIR}/{name}")).unwrap_or_default()
}

fn is_root() -> bool {
    run("id", &["-u"]).map(|s| s.trim() == "0").unwrap_or(false)
}

fn answer() -> Result<Vec<String>, String> {
    let addr = crate::ups::nut_addr().unwrap_or_else(|| "127.0.0.1:3493".into());
    let list = crate::ups::query(&addr)?;
    Ok(list.iter().map(crate::ups::Ups::summary).collect())
}

/// The whole setup. Returns the lines to print on success.
pub fn setup() -> Result<Vec<String>, String> {
    if !is_root() {
        return Err("run it as root: sudo dockwren setup-ups".into());
    }
    if has_ups_section(&read_or_empty("ups.conf")) {
        let mut out =
            vec!["NUT already has a UPS configured in /etc/nut/ups.conf: not touching it.".into()];
        match answer() {
            Ok(l) if !l.is_empty() => out.extend(l),
            _ => out.push(
                "NUT does not answer yet: check `systemctl status nut-server nut-driver@*`.".into(),
            ),
        }
        return Ok(out);
    }
    let usb = crate::ups::usb_ups(Path::new("/sys/bus/usb/devices"));
    if usb.is_empty() {
        return Err("no UPS found on USB. Connect the UPS's USB data cable to this server and run this again.".into());
    }
    if find(&["upsd"]).is_none() {
        if find(&["apt-get"]).is_none() {
            return Err("NUT is not installed and apt-get is missing: install the packages nut-client and nut-server with your package manager, then run this again.".into());
        }
        run("apt-get", &["install", "-y", "nut-client", "nut-server"])?;
    }
    let scanner = find(&["nut-scanner"]).ok_or("nut-scanner not found (package nut-client)")?;
    let dev = parse_scanner(&run(&scanner, &["-U"])?)
        .ok_or("NUT's scanner did not find a UPS it supports on USB")?;
    let pw = random_hex()?;
    write_private("ups.conf", &ups_conf(&dev))?;
    write_private("upsd.users", &upsd_users(&pw))?;
    write_private(
        "upsmon.conf",
        &upsmon_conf(&read_or_empty("upsmon.conf"), &pw),
    )?;
    write_private("nut.conf", &nut_conf(&read_or_empty("nut.conf")))?;
    // A UPS plugged in before NUT existed has no group access yet: re-apply the udev rules.
    let _ = run("udevadm", &["control", "--reload"]);
    let _ = run(
        "udevadm",
        &[
            "trigger",
            "--subsystem-match=usb",
            &format!("--attr-match=idVendor={}", dev.vendorid),
            "--action=add",
        ],
    );
    let _ = run(
        "systemctl",
        &["enable", "--now", "nut-driver-enumerator.service"],
    );
    let unit = format!("nut-driver@{NAME}.service");
    run(
        "systemctl",
        &["enable", "nut-server.service", "nut-monitor.service"],
    )
    .ok();
    if run("systemctl", &["restart", &unit]).is_err() {
        // Older NUT without per-UPS driver units.
        run("upsdrvctl", &["start"]).ok();
    }
    run(
        "systemctl",
        &["restart", "nut-server.service", "nut-monitor.service"],
    )?;
    std::thread::sleep(std::time::Duration::from_secs(4));
    let mut out = vec![format!(
        "NUT configured for {} ({}:{}) as `{NAME}`.",
        usb[0], dev.vendorid, dev.productid
    )];
    match answer() {
        Ok(l) if !l.is_empty() => out.extend(l),
        _ => {
            return Err(format!(
                "NUT is configured but does not answer yet. Check: systemctl status {unit} nut-server"
            ));
        }
    }
    out.push("The server now shuts down cleanly when the UPS battery runs low (NUT upsmon). Dockwren shows the UPS under Settings, System.".into());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCAN: &str = "Scanning USB bus.\n[nutdev1]\n\tdriver = \"usbhid-ups\"\n\tport = \"auto\"\n\tvendorid = \"0463\"\n\tproductid = \"FFFF\"\n\tproduct = \"Eaton 5E\"\n\tserial = \"BLANK\"\n\tvendor = \"EATON\"\n[nutdev2]\n\tdriver = \"other\"\n";

    #[test]
    fn parses_the_first_scanner_device() {
        let d = parse_scanner(SCAN).unwrap();
        assert_eq!(
            d,
            Device {
                driver: "usbhid-ups".into(),
                port: "auto".into(),
                vendorid: "0463".into(),
                productid: "ffff".into()
            }
        );
        assert_eq!(parse_scanner("Scanning USB bus.\n"), None);
        // Values that could break out of the config file are refused.
        let bad = SCAN.replace("usbhid-ups", "x\\n[evil]");
        assert_eq!(parse_scanner(&bad), None);
        let bad = SCAN.replace("\"0463\"", "\"04 3\"");
        assert_eq!(parse_scanner(&bad), None);
    }

    #[test]
    fn config_files_have_the_right_shape() {
        let d = parse_scanner(SCAN).unwrap();
        let c = ups_conf(&d);
        assert!(
            c.contains("[ups]")
                && c.contains("driver = usbhid-ups")
                && c.contains("productid = ffff")
        );
        assert!(has_ups_section(&c));
        assert!(!has_ups_section("# [commented]\nmaxretry = 3\n"));
        assert!(upsd_users("pw1").contains("password = pw1"));
        let m = upsmon_conf("# x\nMONITOR old@h 1 u p primary\nMINSUPPLIES 1\n", "pw2");
        assert_eq!(m.matches("MONITOR ").count(), 1);
        assert!(m.contains("MONITOR ups@localhost 1 upsmon pw2 primary"));
        assert!(m.contains("MINSUPPLIES 1"));
        let n = nut_conf("MODE=none\n# keep\n");
        assert_eq!(n.matches("MODE=").count(), 1);
        assert!(n.contains("MODE=standalone") && n.contains("# keep"));
    }
}
