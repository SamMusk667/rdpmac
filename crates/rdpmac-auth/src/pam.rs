//! Password validation through PAM, which on macOS reaches OpenDirectory for local and
//! directory accounts. Uses the OpenPAM ABI shipped with the system; no crate dependency.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fmt;
use std::ptr;

use async_trait::async_trait;
use ironrdp_server::{CredentialDecision, CredentialValidationError, CredentialValidator, Credentials};
use tracing::{debug, warn};

use crate::bare_username;

const PAM_SUCCESS: c_int = 0;
const PAM_BUF_ERR: c_int = 5;
const PAM_CONV_ERR: c_int = 6;
const PAM_PERM_DENIED: c_int = 7;
const PAM_MAXTRIES: c_int = 8;
const PAM_AUTH_ERR: c_int = 9;
const PAM_NEW_AUTHTOK_REQD: c_int = 10;
const PAM_CRED_INSUFFICIENT: c_int = 11;
const PAM_AUTHINFO_UNAVAIL: c_int = 12;
const PAM_USER_UNKNOWN: c_int = 13;
const PAM_ACCT_EXPIRED: c_int = 17;

const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

type ConvFn = unsafe extern "C" fn(c_int, *mut *const PamMessage, *mut *mut PamResponse, *mut c_void) -> c_int;

#[repr(C)]
struct PamConv {
    conv: Option<ConvFn>,
    appdata_ptr: *mut c_void,
}

type PamHandle = c_void;

#[link(name = "pam")]
extern "C" {
    fn pam_start(service: *const c_char, user: *const c_char, conv: *const PamConv, handle: *mut *mut PamHandle) -> c_int;
    fn pam_authenticate(handle: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_acct_mgmt(handle: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_end(handle: *mut PamHandle, status: c_int) -> c_int;
    fn pam_strerror(handle: *mut PamHandle, errnum: c_int) -> *const c_char;
}

/// Answers every password prompt with the password the client sent; PAM frees the responses.
unsafe extern "C" fn conversation(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata: *mut c_void,
) -> c_int {
    if num_msg <= 0 || msg.is_null() || resp.is_null() || appdata.is_null() {
        return PAM_CONV_ERR;
    }
    let password = &*(appdata as *const CString);
    let count = num_msg as usize;
    let responses = libc::calloc(count, std::mem::size_of::<PamResponse>()) as *mut PamResponse;
    if responses.is_null() {
        return PAM_BUF_ERR;
    }
    for i in 0..count {
        let message = *msg.add(i);
        if message.is_null() {
            continue;
        }
        if matches!((*message).msg_style, PAM_PROMPT_ECHO_OFF | PAM_PROMPT_ECHO_ON) {
            let copy = libc::strdup(password.as_ptr());
            if copy.is_null() {
                for j in 0..i {
                    libc::free((*responses.add(j)).resp as *mut c_void);
                }
                libc::free(responses as *mut c_void);
                return PAM_BUF_ERR;
            }
            (*responses.add(i)).resp = copy;
        }
    }
    *resp = responses;
    PAM_SUCCESS
}

#[derive(Debug)]
pub struct PamBackendError(String);

impl fmt::Display for PamBackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PamBackendError {}

enum Outcome {
    Accepted,
    Rejected(String),
}

fn describe(handle: *mut PamHandle, code: c_int) -> String {
    let text = unsafe { pam_strerror(handle, code) };
    if text.is_null() {
        return format!("pam error {code}");
    }
    unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned()
}

fn authenticate(service: &str, user: &str, password: &str) -> Result<Outcome, PamBackendError> {
    if user.is_empty() || user.chars().any(char::is_control) {
        return Ok(Outcome::Rejected("invalid username".into()));
    }
    let service = CString::new(service).map_err(|_| PamBackendError("service name contains NUL".into()))?;
    let user_c = match CString::new(user) {
        Ok(u) => u,
        Err(_) => return Ok(Outcome::Rejected("username contains NUL".into())),
    };
    let password_c = match CString::new(password) {
        Ok(p) => p,
        Err(_) => return Ok(Outcome::Rejected("password contains NUL".into())),
    };
    let conv = PamConv {
        conv: Some(conversation),
        appdata_ptr: &password_c as *const CString as *mut c_void,
    };
    let mut handle: *mut PamHandle = ptr::null_mut();
    let started = unsafe { pam_start(service.as_ptr(), user_c.as_ptr(), &conv, &mut handle) };
    if started != PAM_SUCCESS || handle.is_null() {
        return Err(PamBackendError(format!("pam_start failed: {}", describe(ptr::null_mut(), started))));
    }
    let mut code = unsafe { pam_authenticate(handle, 0) };
    if code == PAM_SUCCESS {
        code = unsafe { pam_acct_mgmt(handle, 0) };
    }
    let reason = describe(handle, code);
    unsafe { pam_end(handle, code) };
    match code {
        PAM_SUCCESS => Ok(Outcome::Accepted),
        PAM_AUTH_ERR
        | PAM_USER_UNKNOWN
        | PAM_PERM_DENIED
        | PAM_MAXTRIES
        | PAM_CRED_INSUFFICIENT
        | PAM_AUTHINFO_UNAVAIL
        | PAM_ACCT_EXPIRED
        | PAM_NEW_AUTHTOK_REQD => Ok(Outcome::Rejected(reason)),
        _ => Err(PamBackendError(format!("pam failure {code}: {reason}"))),
    }
}

/// Validates the client's username and password against a PAM service.
///
/// `checkpw` is the macOS service meant for exactly this: OpenDirectory authentication plus
/// account checks, without Kerberos or NTLM detours.
pub struct PamValidator {
    service: String,
}

impl PamValidator {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }
}

#[async_trait]
impl CredentialValidator for PamValidator {
    async fn validate(&self, credentials: &Credentials) -> Result<CredentialDecision, CredentialValidationError> {
        let service = self.service.clone();
        let user = bare_username(&credentials.username).to_owned();
        let password = credentials.password.clone();
        let outcome = tokio::task::spawn_blocking(move || authenticate(&service, &user, &password))
            .await
            .map_err(CredentialValidationError::new)?
            .map_err(CredentialValidationError::new)?;
        match outcome {
            Outcome::Accepted => {
                debug!(user = %bare_username(&credentials.username), "pam accepted");
                Ok(CredentialDecision::Accept)
            }
            Outcome::Rejected(reason) => {
                warn!(user = %bare_username(&credentials.username), %reason, "pam rejected");
                Ok(CredentialDecision::Reject)
            }
        }
    }
}
