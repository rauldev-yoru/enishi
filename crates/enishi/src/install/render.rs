//! Pure generators for the install artifacts (testable on every OS): the systemd user unit,
//! the Task Scheduler XML, the PE subsystem flip and the printed elevated commands.

use std::path::Path;

use anyhow::{Result, bail};

/// Quote one argument for a systemd `ExecStart=` line (`%` specifiers and `$` variables
/// escaped so paths are taken literally).
pub fn sd_exec_arg(s: &str) -> String {
    let mut out = String::from('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Quote an `Environment=` assignment (specifiers are expanded there, variables are not).
fn sd_env(key: &str, value: &str) -> String {
    let mut out = format!("\"{key}=");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `~/.config/systemd/user/<name>.service`. A non-default config is pinned with
/// `Environment=ENISHI_CONFIG=…`, which `enishi up` (and anything it runs) inherits.
pub fn systemd_unit(bin: &Path, config: Option<&Path>) -> String {
    let mut s = String::from(
        "# Written by `enishi install`; removed by `enishi uninstall`.\n\
         [Unit]\n\
         Description=enishi device link daemon\n\
         Documentation=https://github.com/rauldev-yoru/enishi\n\
         \n\
         [Service]\n\
         Type=simple\n",
    );
    s.push_str(&format!(
        "ExecStart={} up --log-file\n",
        sd_exec_arg(&bin.to_string_lossy())
    ));
    if let Some(c) = config {
        s.push_str(&format!(
            "Environment={}\n",
            sd_env("ENISHI_CONFIG", &c.to_string_lossy())
        ));
    }
    s.push_str(
        "WorkingDirectory=~\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
    );
    s
}

pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// Quote one argument for a Windows command line (CommandLineToArgvW rules).
pub fn win_arg(s: &str) -> String {
    if !s.is_empty() && !s.contains([' ', '\t', '"']) {
        return s.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for c in s.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// Arguments of the logon task's action.
pub fn task_arguments(config: Option<&Path>) -> String {
    let mut a = String::from("up --log-file");
    if let Some(c) = config {
        a.push_str(" --config ");
        a.push_str(&win_arg(&c.to_string_lossy()));
    }
    a
}

/// Task Scheduler 1.2 definition: at logon of `user`, interactive token (runs in the
/// user's desktop session, only while logged on), no time limit, restart on failure,
/// normal priority (the default 7 would make every `exec` child below-normal).
pub fn task_xml(name: &str, user: &str, command: &Path, args: &str, workdir: &Path) -> String {
    let user = xml_escape(user);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>enishi device link daemon. Written by `enishi install`; removed by `enishi uninstall`.</Description>
    <URI>\{name}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{args}</Arguments>
      <WorkingDirectory>{workdir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        name = xml_escape(name),
        command = xml_escape(&command.to_string_lossy()),
        args = xml_escape(args),
        workdir = xml_escape(&workdir.to_string_lossy()),
    )
}

/// One-shot task for the detached updater (Windows fallback when the updater cannot break
/// away from the daemon's job): no trigger (started with `schtasks /Run`), interactive
/// token, 30 min limit; the updater deletes it when done.
pub fn update_task_xml(user: &str, command: &Path, args: &str, workdir: &Path) -> String {
    let user = xml_escape(user);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>enishi one-shot updater. Written by `enishi update`; deletes itself.</Description>
  </RegistrationInfo>
  <Triggers />
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT30M</ExecutionTimeLimit>
    <Priority>5</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{args}</Arguments>
      <WorkingDirectory>{workdir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        command = xml_escape(&command.to_string_lossy()),
        args = xml_escape(args),
        workdir = xml_escape(&workdir.to_string_lossy()),
    )
}

/// UTF-16LE with BOM, as `schtasks /Create /XML` expects for `encoding="UTF-16"`.
pub fn utf16le_bom(s: &str) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFE];
    for u in s.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

const IMAGE_SUBSYSTEM_WINDOWS_GUI: u16 = 2;
const IMAGE_SUBSYSTEM_WINDOWS_CUI: u16 = 3;

/// Turn a console (CUI) PE image into a GUI-subsystem one in place — what `editbin
/// /SUBSYSTEM:WINDOWS` does. Windows then starts it without allocating a console, so the
/// logon task runs the daemon with no window at all. Only the 2-byte Subsystem field of the
/// optional header changes (the PE checksum is not checked for user-mode images).
pub fn pe_to_gui(image: &mut [u8]) -> Result<()> {
    let u16_at = |b: &[u8], o: usize| b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]));
    let u32_at = |b: &[u8], o: usize| {
        b.get(o..o + 4)
            .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    };
    if image.get(0..2) != Some(b"MZ") {
        bail!("not a PE image (no MZ header)");
    }
    let Some(pe) = u32_at(image, 0x3C).map(|v| v as usize) else {
        bail!("truncated PE image");
    };
    if image.get(pe..pe + 4) != Some(b"PE\0\0") {
        bail!("not a PE image (no PE signature)");
    }
    let opt = pe + 4 + 20;
    match u16_at(image, opt) {
        Some(0x10b) | Some(0x20b) => {}
        _ => bail!("unknown PE optional header magic"),
    }
    let sub = opt + 68;
    match u16_at(image, sub) {
        Some(IMAGE_SUBSYSTEM_WINDOWS_CUI) | Some(IMAGE_SUBSYSTEM_WINDOWS_GUI) => {}
        Some(other) => bail!("unexpected PE subsystem {other}"),
        None => bail!("truncated PE image"),
    }
    image[sub..sub + 2].copy_from_slice(&IMAGE_SUBSYSTEM_WINDOWS_GUI.to_le_bytes());
    Ok(())
}

/// PowerShell single-quoted literal.
pub fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// One line that removes, elevated, our named rule and every rule bound to the given
/// program paths (the ones Windows' "allow access" prompt creates). Needs no enishi binary,
/// so uninstall can print it after deleting the files.
pub fn elevated_firewall_remove_cmd(rule: &str, programs: &[&Path]) -> String {
    let mut line = format!("/c netsh advfirewall firewall delete rule name=\"{rule}\"");
    for p in programs {
        line.push_str(&format!(
            " & netsh advfirewall firewall delete rule name=all program=\"{}\"",
            p.display()
        ));
    }
    format!(
        "Start-Process -Verb RunAs -Wait -FilePath cmd -ArgumentList {}",
        ps_quote(&line)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn update_task_is_one_shot() {
        let x = update_task_xml(
            r"PC\raul",
            Path::new(r"C:\Users\raul\AppData\Local\enishi\update\enishi\enishi-updater.exe"),
            r#"update --stage2 --name enishi --from-file "C:\a b\enishi-new.exe""#,
            Path::new(r"C:\Users\raul"),
        );
        assert!(x.contains("<Triggers />"), "no trigger: started by /Run");
        assert!(x.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(x.contains("<ExecutionTimeLimit>PT30M</ExecutionTimeLimit>"));
        assert!(
            x.contains("--from-file &quot;C:\\a b\\enishi-new.exe&quot;"),
            "{x}"
        );
        assert!(!x.contains("RestartOnFailure"));
    }

    #[test]
    fn unit_quotes_and_pins_config() {
        let u = systemd_unit(
            Path::new("/home/a b/.local/bin/enishi"),
            Some(Path::new("/srv/x%y/enishi.toml")),
        );
        assert!(
            u.contains("ExecStart=\"/home/a b/.local/bin/enishi\" up --log-file\n"),
            "{u}"
        );
        assert!(
            u.contains("Environment=\"ENISHI_CONFIG=/srv/x%%y/enishi.toml\"\n"),
            "{u}"
        );
        assert!(u.contains("Restart=on-failure\n") && u.contains("WantedBy=default.target"));
        let u = systemd_unit(Path::new("/b/enishi"), None);
        assert!(!u.contains("ENISHI_CONFIG"));
        assert_eq!(sd_exec_arg("a$b\"c\\"), "\"a$$b\\\"c\\\\\"");
    }

    #[test]
    fn task_definition() {
        let x = task_xml(
            "enishi-test",
            "PC\\raúl & co",
            Path::new(r"C:\Users\r\AppData\Local\Programs\enishi\enishiw.exe"),
            &task_arguments(Some(&PathBuf::from(r"C:\Users\r\my cfg\enishi.toml"))),
            Path::new(r"C:\Users\r"),
        );
        assert!(x.contains("<URI>\\enishi-test</URI>"));
        assert!(x.contains("<UserId>PC\\raúl &amp; co</UserId>"));
        assert!(x.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(x.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(x.contains("<RestartOnFailure>"));
        assert!(x.contains(
            "<Arguments>up --log-file --config &quot;C:\\Users\\r\\my cfg\\enishi.toml&quot;</Arguments>"
        ));
        assert_eq!(task_arguments(None), "up --log-file");
        let b = utf16le_bom("<a/>");
        assert_eq!(&b[..4], &[0xFF, 0xFE, b'<', 0]);
    }

    #[test]
    fn windows_arg_quoting() {
        assert_eq!(win_arg("plain"), "plain");
        assert_eq!(win_arg(r"C:\a b\"), r#""C:\a b\\""#);
        assert_eq!(win_arg(r#"x"y"#), r#""x\"y""#);
        assert_eq!(win_arg(""), "\"\"");
        assert_eq!(ps_quote(r"C:\Users\o'neil"), r"'C:\Users\o''neil'");
        assert_eq!(
            elevated_firewall_remove_cmd("enishi", &[Path::new(r"C:\P\enishiw.exe")]),
            r#"Start-Process -Verb RunAs -Wait -FilePath cmd -ArgumentList '/c netsh advfirewall firewall delete rule name="enishi" & netsh advfirewall firewall delete rule name=all program="C:\P\enishiw.exe"'"#
        );
    }

    fn fake_pe(subsystem: u16) -> Vec<u8> {
        let mut b = vec![0u8; 0x200];
        b[0..2].copy_from_slice(b"MZ");
        b[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        b[0x80..0x84].copy_from_slice(b"PE\0\0");
        let opt = 0x80 + 24;
        b[opt..opt + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        b[opt + 68..opt + 70].copy_from_slice(&subsystem.to_le_bytes());
        b
    }

    #[test]
    fn pe_subsystem_flip() {
        let mut b = fake_pe(3);
        let before = b.clone();
        pe_to_gui(&mut b).unwrap();
        let opt = 0x80 + 24;
        assert_eq!(u16::from_le_bytes([b[opt + 68], b[opt + 69]]), 2);
        let changed: Vec<usize> = (0..b.len()).filter(|&i| b[i] != before[i]).collect();
        assert_eq!(changed, vec![opt + 68]);
        assert!(pe_to_gui(&mut fake_pe(2)).is_ok(), "already GUI is fine");
        assert!(pe_to_gui(&mut fake_pe(1)).is_err(), "native driver refused");
        assert!(pe_to_gui(&mut b"\x7fELF....".to_vec()).is_err());
    }
}
