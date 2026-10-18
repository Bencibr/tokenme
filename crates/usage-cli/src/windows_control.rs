#![cfg(windows)]

use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use std::thread::{self, JoinHandle};

use usage_device_sync::ClientTransportCancellation;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::{
    AddAccessAllowedAce, GetLengthSid, GetTokenInformation, InitializeAcl,
    InitializeSecurityDescriptor, SetSecurityDescriptorDacl, ACL, ACL_REVISION,
    SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
    TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, ReadFile, WriteFile, FILE_ALL_ACCESS};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;

const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
const PIPE_WAIT: u32 = 0x0000_0000;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const OPEN_EXISTING: u32 = 3;
const ERROR_PIPE_CONNECTED: u32 = 535;
const ERROR_PIPE_BUSY: u32 = 231;
const ERROR_FILE_NOT_FOUND: u32 = 2;

pub struct ControlServer {
    name: String,
    join: Option<JoinHandle<Result<(), String>>>,
}

pub fn control_name(db_path: &Path) -> String {
    let digest = usage_device_sync::hash_bytes(db_path.to_string_lossy().as_bytes());
    format!(r"\\.\pipe\tokenme-secondary-{}", hex_bytes(&digest[..16]))
}

pub fn start(
    db_path: &Path,
    stop: Arc<AtomicBool>,
    cancellation: ClientTransportCancellation,
) -> Result<ControlServer, String> {
    let name = control_name(db_path);
    if request_name(&name, "status").is_ok() {
        return Err("secondary runtime is already running".into());
    }
    let pipe = create_server(&name)?;
    let thread_name = name.clone();
    let pipe = pipe as usize;
    let join = thread::spawn(move || run_server(thread_name, pipe, stop, Some(cancellation)));
    Ok(ControlServer { name, join: Some(join) })
}

impl ControlServer {
    pub fn shutdown(mut self) -> Result<(), String> {
        if self.join.as_ref().is_some_and(|join| !join.is_finished()) {
            let _ = request_name(&self.name, "shutdown");
        }
        self.join
            .take()
            .expect("control server join handle is present")
            .join()
            .map_err(|_| "Windows named-pipe control thread panicked".to_string())?
    }
}

pub fn request(db_path: &Path, command: &str) -> Result<String, String> {
    request_name(&control_name(db_path), command)
}

fn request_name(name: &str, command: &str) -> Result<String, String> {
    let wide_name = wide(name);
    let mut pipe = INVALID_HANDLE_VALUE;
    for _ in 0..40 {
        pipe = unsafe {
            CreateFileW(
                wide_name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if pipe != INVALID_HANDLE_VALUE { break; }
        let error = unsafe { GetLastError() };
        if error != ERROR_PIPE_BUSY && error != ERROR_FILE_NOT_FOUND { break; }
        unsafe { WaitNamedPipeW(wide_name.as_ptr(), 50); }
    }
    if pipe == INVALID_HANDLE_VALUE {
        return Err(win_error("open private secondary named pipe"));
    }

    let mut command_bytes = command.as_bytes().to_vec();
    command_bytes.push(b'\n');
    let result = (|| {
        write_all(pipe, &command_bytes)?;
        let mut response = [0u8; 64];
        let count = read_once(pipe, &mut response)?;
        Ok(String::from_utf8_lossy(&response[..count]).trim().to_string())
    })();
    unsafe { CloseHandle(pipe); }
    result
}

fn run_server(
    name: String,
    pipe_bits: usize,
    stop: Arc<AtomicBool>,
    cancellation: Option<ClientTransportCancellation>,
) -> Result<(), String> {
    let mut pipe = pipe_bits as HANDLE;
    loop {
        let connected = unsafe { ConnectNamedPipe(pipe, null_mut()) };
        if connected == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
            let error = win_error("wait for private secondary named-pipe client");
            unsafe { CloseHandle(pipe); }
            return Err(error);
        }
        let mut command = [0u8; 64];
        let count = match read_once(pipe, &mut command) {
            Ok(count) => count,
            Err(error) => {
                unsafe {
                    DisconnectNamedPipe(pipe);
                    CloseHandle(pipe);
                }
                return Err(error);
            }
        };
        let command = String::from_utf8_lossy(&command[..count]).trim().to_string();
        let terminal = match command.as_str() {
            "status" => {
                write_all(pipe, b"running\n")?;
                false
            }
            "stop" => {
                stop.store(true, Ordering::Release);
                if let Some(cancellation) = &cancellation { cancellation.cancel(); }
                write_all(pipe, b"stopping\n")?;
                true
            }
            "shutdown" => {
                write_all(pipe, b"closing\n")?;
                true
            }
            _ => {
                write_all(pipe, b"unknown command\n")?;
                false
            }
        };
        unsafe {
            DisconnectNamedPipe(pipe);
            CloseHandle(pipe);
        }
        if terminal || stop.load(Ordering::Acquire) { return Ok(()); }
        pipe = create_server(&name)? as usize as HANDLE;
    }
}

fn create_server(name: &str) -> Result<HANDLE, String> {
    let (token, sid_copy, acl, mut descriptor) = private_security()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
        bInheritHandle: 0,
    };
    let wide_name = wide(name);
    let pipe = unsafe {
        CreateNamedPipeW(
            wide_name.as_ptr(),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            4 * 1024,
            4 * 1024,
            1_000,
            &attributes,
        )
    };
    unsafe { CloseHandle(token); }
    drop(sid_copy);
    drop(acl);
    if pipe == INVALID_HANDLE_VALUE {
        return Err(win_error("create private secondary named pipe"));
    }
    Ok(pipe)
}

fn private_security() -> Result<(HANDLE, Vec<u8>, Vec<u8>, SECURITY_DESCRIPTOR), String> {
    let mut token: HANDLE = null_mut();
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(win_error("open current Windows token"));
        }
    }
    let mut needed = 0u32;
    unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut needed); }
    if needed == 0 {
        unsafe { CloseHandle(token); }
        return Err(win_error("query current Windows SID size"));
    }
    let mut buffer = vec![0u8; needed as usize];
    unsafe {
        if GetTokenInformation(token, TokenUser, buffer.as_mut_ptr().cast(), needed, &mut needed) == 0 {
            let error = win_error("query current Windows SID");
            CloseHandle(token);
            return Err(error);
        }
        let token_user = &*(buffer.as_ptr().cast::<TOKEN_USER>());
        let sid_len = GetLengthSid(token_user.User.Sid);
        if sid_len == 0 {
            CloseHandle(token);
            return Err(win_error("read current Windows SID"));
        }
        let sid = std::slice::from_raw_parts(token_user.User.Sid.cast::<u8>(), sid_len as usize);
        let mut sid_copy = sid.to_vec();
        let mut acl = vec![0u8; 1024];
        if InitializeAcl(acl.as_mut_ptr().cast::<ACL>(), acl.len() as u32, ACL_REVISION) == 0
            || AddAccessAllowedAce(acl.as_mut_ptr().cast::<ACL>(), ACL_REVISION, FILE_ALL_ACCESS, sid_copy.as_mut_ptr().cast()) == 0
        {
            CloseHandle(token);
            return Err(win_error("build private named-pipe ACL"));
        }
        let mut descriptor: SECURITY_DESCRIPTOR = std::mem::zeroed();
        if InitializeSecurityDescriptor((&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(), SECURITY_DESCRIPTOR_REVISION) == 0
            || SetSecurityDescriptorDacl((&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(), 1, acl.as_ptr().cast::<ACL>(), 0) == 0
        {
            CloseHandle(token);
            return Err(win_error("build private named-pipe security descriptor"));
        }
        Ok((token, sid_copy, acl, descriptor))
    }
}

fn read_once(pipe: HANDLE, buffer: &mut [u8]) -> Result<usize, String> {
    let mut count = 0u32;
    let ok = unsafe { ReadFile(pipe, buffer.as_mut_ptr().cast(), buffer.len() as u32, &mut count, null_mut()) };
    if ok == 0 { return Err(win_error("read private secondary named pipe")); }
    Ok(count as usize)
}

fn write_all(pipe: HANDLE, bytes: &[u8]) -> Result<(), String> {
    let mut written = 0u32;
    let ok = unsafe { WriteFile(pipe, bytes.as_ptr().cast(), bytes.len() as u32, &mut written, null_mut()) };
    if ok == 0 || written as usize != bytes.len() { return Err(win_error("write private secondary named pipe")); }
    Ok(())
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn win_error(action: &str) -> String {
    let code = unsafe { GetLastError() };
    format!("{action} failed (Win32 error {code})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_pipe_round_trip_and_stop() {
        let db_path = std::env::temp_dir().join(format!("tokenme-pipe-test-{}\\index.db", std::process::id()));
        let name = control_name(&db_path);
        let pipe = create_server(&name).expect("create test pipe");
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let pipe_bits = pipe as usize;
        let join = thread::spawn(move || run_server(name, pipe_bits, server_stop, None));
        thread::sleep(std::time::Duration::from_millis(20));
        if let Some(ms) = std::env::var("TOKENME_PIPE_TEST_HOLD_MS").ok().and_then(|value| value.parse::<u64>().ok()).filter(|ms| *ms > 0) {
            println!("pipe: {}", control_name(&db_path));
            thread::sleep(std::time::Duration::from_millis(ms));
        }
        assert_eq!(request_name(&control_name(&db_path), "status").unwrap(), "running");
        assert_eq!(request_name(&control_name(&db_path), "stop").unwrap(), "stopping");
        assert!(stop.load(Ordering::Acquire));
        join.join().unwrap().unwrap();
    }
}
