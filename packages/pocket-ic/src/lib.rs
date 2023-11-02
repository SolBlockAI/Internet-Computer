use candid::utils::{ArgumentDecoder, ArgumentEncoder};
use candid::{decode_args, encode_args, Principal};
use common::blob::{BlobCompression, BlobId};
use common::rest::{
    RawAddCycles, RawCanisterCall, RawCanisterId, RawCheckpoint, RawSetStableMemory,
};
use ic_cdk::api::management_canister::main::{
    CanisterId, CanisterIdRecord, CanisterInstallMode, CanisterSettings, CreateCanisterArgument,
    InstallCodeArgument,
};
use reqwest::Url;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

pub mod common;
pub mod pocket_ic_v2;

pub use pocket_ic_v2::PocketIcV2;

const LOCALHOST: &str = "127.0.0.1";
type InstanceId = String;

// ======================================================================================================
// Code borrowed from https://github.com/dfinity/test-state-machine-client/blob/main/src/lib.rs
// The StateMachine struct is renamed to `PocketIc` and given new interface.
pub struct PocketIc {
    pub instance_id: InstanceId,
    // The PocketIC server's base address plus "/instances/<instance_id>/".
    // All communication with this IC instance goes through this endpoint.
    instance_url: Url,
    server_url: Url,
    reqwest_client: reqwest::blocking::Client,
}

impl PocketIc {
    pub fn new() -> Self {
        let server_url = start_or_reuse_server();
        let reqwest_client = reqwest::blocking::Client::new();
        let instance_id = reqwest_client
            .post(server_url.join("instances/").unwrap())
            .send()
            .expect("Failed to get result")
            .text()
            .expect("Failed to get text");
        let instance_url = server_url
            .join("instances/")
            .unwrap()
            .join(&format!("{instance_id}/"))
            .unwrap();

        Self {
            instance_id,
            instance_url,
            server_url,
            reqwest_client,
        }
    }

    pub fn new_from_snapshot<S: AsRef<str> + std::fmt::Display + serde::Serialize + Copy>(
        name: S,
    ) -> Result<Self, String> {
        let server_url = start_or_reuse_server();
        let reqwest_client = reqwest::blocking::Client::new();
        let cp = RawCheckpoint {
            checkpoint_name: name.to_string(),
        };
        let response = reqwest_client
            .post(server_url.join("instances/").unwrap())
            .json(&cp)
            .send()
            .expect("Failed to get result");
        let status = response.status();
        match status {
            reqwest::StatusCode::CREATED => {
                let instance_id = response.text().expect("Failed to get text");
                let instance_url = server_url
                    .join("instances/")
                    .unwrap()
                    .join(&format!("{instance_id}/"))
                    .unwrap();

                Ok(Self {
                    instance_id,
                    instance_url,
                    server_url,
                    reqwest_client,
                })
            }
            reqwest::StatusCode::BAD_REQUEST => {
                Err(format!("Could not find snapshot named '{name}'."))
            }
            _ => Err(format!(
                "The PocketIC server returned status code {}: {:?}!",
                status,
                response.text()
            )),
        }
    }

    pub fn list_instances() -> Vec<InstanceId> {
        let url = start_or_reuse_server().join("instances/").unwrap();
        let response = reqwest::blocking::Client::new()
            .get(url)
            .send()
            .expect("Failed to get result")
            .text()
            .expect("Failed to get text");
        response.split(", ").map(String::from).collect()
    }

    pub fn send_request(&self, request: Request) -> String {
        self.reqwest_client
            .post(self.instance_url.clone())
            .json(&request)
            .send()
            .expect("Failed to get result")
            .text()
            .expect("Failed to get text")
    }

    fn set_blob_store(&self, blob: Vec<u8>, compression: BlobCompression) -> BlobId {
        let mut request = self
            .reqwest_client
            .post(self.server_url.join("blobstore/").unwrap())
            .body(blob);
        if compression == BlobCompression::Gzip {
            request = request.header(reqwest::header::CONTENT_ENCODING, "gzip");
        }
        let blob_id = request
            .send()
            .expect("Failed to get response")
            .text()
            .expect("Failed to get text");

        let hash_vec = hex::decode(blob_id).expect("Failed to decode hex");
        let hash: Result<[u8; 32], Vec<u8>> = hash_vec.try_into();
        BlobId(hash.expect("Invalid hash"))
    }

    // TODO: Add a function that separates those two
    pub fn tick_and_create_checkpoint(&self, name: &str) -> String {
        let url = self
            .instance_url
            .join("tick_and_create_checkpoint/")
            .unwrap();
        let cp = RawCheckpoint {
            checkpoint_name: name.to_string(),
        };
        self.reqwest_client
            .post(url)
            .json(&cp)
            .send()
            .expect("Failed to get result")
            .text()
            .expect("Failed to get text")
    }
    // ------------------------------------------------------------------

    pub fn update_call(
        &self,
        canister_id: Principal,
        sender: Principal,
        method: &str,
        payload: Vec<u8>,
    ) -> Result<WasmResult, UserError> {
        self.call_state_machine(Request::CanisterUpdateCall(RawCanisterCall {
            sender: sender.as_slice().to_vec(),
            canister_id: canister_id.as_slice().to_vec(),
            method: method.to_string(),
            payload,
        }))
    }

    pub fn query_call(
        &self,
        canister_id: Principal,
        sender: Principal,
        method: &str,
        payload: Vec<u8>,
    ) -> Result<WasmResult, UserError> {
        self.call_state_machine(Request::CanisterQueryCall(RawCanisterCall {
            sender: sender.as_slice().to_vec(),
            canister_id: canister_id.as_slice().to_vec(),
            method: method.to_string(),
            payload,
        }))
    }

    pub fn root_key(&self) -> Vec<u8> {
        self.call_state_machine(Request::RootKey)
    }

    pub fn create_canister(&self, sender: Option<Principal>) -> CanisterId {
        let CanisterIdRecord { canister_id } = call_candid_as(
            self,
            Principal::management_canister(),
            sender.unwrap_or(Principal::anonymous()),
            "create_canister",
            (CreateCanisterArgument { settings: None },),
        )
        .map(|(x,)| x)
        .unwrap();
        canister_id
    }

    pub fn create_canister_with_settings(
        &self,
        settings: Option<CanisterSettings>,
        sender: Option<Principal>,
    ) -> CanisterId {
        let CanisterIdRecord { canister_id } = call_candid_as(
            self,
            Principal::management_canister(),
            sender.unwrap_or(Principal::anonymous()),
            "create_canister",
            (CreateCanisterArgument { settings },),
        )
        .map(|(x,)| x)
        .unwrap();
        canister_id
    }

    pub fn install_canister(
        &self,
        canister_id: CanisterId,
        wasm_module: Vec<u8>,
        arg: Vec<u8>,
        sender: Option<Principal>,
    ) {
        call_candid_as::<(InstallCodeArgument,), ()>(
            self,
            Principal::management_canister(),
            sender.unwrap_or(Principal::anonymous()),
            "install_code",
            (InstallCodeArgument {
                mode: CanisterInstallMode::Install,
                canister_id,
                wasm_module,
                arg,
            },),
        )
        .unwrap();
    }

    pub fn upgrade_canister(
        &self,
        canister_id: CanisterId,
        wasm_module: Vec<u8>,
        arg: Vec<u8>,
        sender: Option<Principal>,
    ) -> Result<(), CallError> {
        call_candid_as::<(InstallCodeArgument,), ()>(
            self,
            Principal::management_canister(),
            sender.unwrap_or(Principal::anonymous()),
            "install_code",
            (InstallCodeArgument {
                mode: CanisterInstallMode::Upgrade,
                canister_id,
                wasm_module,
                arg,
            },),
        )
    }

    pub fn reinstall_canister(
        &self,
        canister_id: CanisterId,
        wasm_module: Vec<u8>,
        arg: Vec<u8>,
        sender: Option<Principal>,
    ) -> Result<(), CallError> {
        call_candid_as::<(InstallCodeArgument,), ()>(
            self,
            Principal::management_canister(),
            sender.unwrap_or(Principal::anonymous()),
            "install_code",
            (InstallCodeArgument {
                mode: CanisterInstallMode::Reinstall,
                canister_id,
                wasm_module,
                arg,
            },),
        )
    }

    pub fn start_canister(
        &self,
        canister_id: CanisterId,
        sender: Option<Principal>,
    ) -> Result<(), CallError> {
        call_candid_as::<(CanisterIdRecord,), ()>(
            self,
            Principal::management_canister(),
            sender.unwrap_or(Principal::anonymous()),
            "start_canister",
            (CanisterIdRecord { canister_id },),
        )
    }

    pub fn stop_canister(
        &self,
        canister_id: CanisterId,
        sender: Option<Principal>,
    ) -> Result<(), CallError> {
        call_candid_as::<(CanisterIdRecord,), ()>(
            self,
            Principal::management_canister(),
            sender.unwrap_or(Principal::anonymous()),
            "stop_canister",
            (CanisterIdRecord { canister_id },),
        )
    }

    pub fn delete_canister(
        &self,
        canister_id: CanisterId,
        sender: Option<Principal>,
    ) -> Result<(), CallError> {
        call_candid_as::<(CanisterIdRecord,), ()>(
            self,
            Principal::management_canister(),
            sender.unwrap_or(Principal::anonymous()),
            "delete_canister",
            (CanisterIdRecord { canister_id },),
        )
    }

    pub fn canister_exists(&self, canister_id: Principal) -> bool {
        self.call_state_machine(Request::CanisterExists(RawCanisterId::from(canister_id)))
    }

    pub fn time(&self) -> SystemTime {
        self.call_state_machine(Request::Time)
    }

    pub fn set_time(&self, time: SystemTime) {
        self.call_state_machine(Request::SetTime(time))
    }

    pub fn advance_time(&self, duration: Duration) {
        self.call_state_machine(Request::AdvanceTime(duration))
    }

    pub fn tick(&self) {
        self.call_state_machine(Request::Tick)
    }

    // TODO: There have been complaints that this function is misleading.
    // We should consider removing it from the interface or refactoring it.

    pub fn run_until_completion(&self, max_ticks: u64) {
        self.call_state_machine(Request::RunUntilCompletion(RunUntilCompletionArg {
            max_ticks,
        }))
    }

    pub fn get_stable_memory(&self, canister_id: Principal) -> Vec<u8> {
        self.call_state_machine(Request::ReadStableMemory(RawCanisterId::from(canister_id)))
    }

    pub fn set_stable_memory(
        &self,
        canister_id: Principal,
        data: Vec<u8>,
        compression: BlobCompression,
    ) -> Result<(), String> {
        let blob_id = self.set_blob_store(data, compression);
        let res = self
            .reqwest_client
            .post(self.instance_url.clone())
            .json(&Request::SetStableMemory(RawSetStableMemory {
                canister_id: canister_id.as_slice().to_vec(),
                blob_id,
            }))
            .send()
            .expect("Failed to get result");
        let status = res.status();
        let text = res.text().expect("Failed to get text");
        match status {
            reqwest::StatusCode::OK => Ok(()),
            _ => Err(format!(
                "The PocketIC server returned status code {}: {:?}!",
                status, text
            )),
        }
    }

    pub fn cycle_balance(&self, canister_id: Principal) -> u128 {
        self.call_state_machine(Request::CyclesBalance(RawCanisterId::from(canister_id)))
    }

    pub fn add_cycles(&self, canister_id: Principal, amount: u128) -> u128 {
        self.call_state_machine(Request::AddCycles(RawAddCycles {
            canister_id: canister_id.as_slice().to_vec(),
            amount,
        }))
    }

    /// Verifies a canister signature. Returns Ok(()) if the signature is valid.
    /// On error, returns a string describing the error.
    pub fn verify_canister_signature(
        &self,
        msg: Vec<u8>,
        sig: Vec<u8>,
        pubkey: Vec<u8>,
        root_pubkey: Vec<u8>,
    ) -> Result<(), String> {
        self.call_state_machine(Request::VerifyCanisterSig(VerifyCanisterSigArg {
            msg,
            sig,
            pubkey,
            root_pubkey,
        }))
    }

    fn call_state_machine<T: DeserializeOwned>(&self, request: Request) -> T {
        let res = self.send_request(request);
        serde_json::from_str(&res).expect("Failed to decode json")
    }
}

impl Default for PocketIc {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PocketIc {
    fn drop(&mut self) {
        let _result = self
            .reqwest_client
            .delete(self.instance_url.clone())
            .send()
            .expect("Failed to delete instance on server");
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    RootKey,
    Time,
    SetTime(SystemTime),
    AdvanceTime(Duration),
    CanisterUpdateCall(RawCanisterCall),
    CanisterQueryCall(RawCanisterCall),
    CanisterExists(RawCanisterId),
    CyclesBalance(RawCanisterId),
    AddCycles(RawAddCycles),
    SetStableMemory(RawSetStableMemory),
    ReadStableMemory(RawCanisterId),
    Tick,
    RunUntilCompletion(RunUntilCompletionArg),
    VerifyCanisterSig(VerifyCanisterSigArg),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct VerifyCanisterSigArg {
    #[serde(with = "base64")]
    pub msg: Vec<u8>,
    #[serde(with = "base64")]
    pub sig: Vec<u8>,
    #[serde(with = "base64")]
    pub pubkey: Vec<u8>,
    #[serde(with = "base64")]
    pub root_pubkey: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct RunUntilCompletionArg {
    // max_ticks until completion must be reached
    pub max_ticks: u64,
}

/// Call a canister candid query method, anonymous.
pub fn query_candid<Input, Output>(
    env: &PocketIc,
    canister_id: Principal,
    method: &str,
    input: Input,
) -> Result<Output, CallError>
where
    Input: ArgumentEncoder,
    Output: for<'a> ArgumentDecoder<'a>,
{
    query_candid_as(env, canister_id, Principal::anonymous(), method, input)
}

/// Call a canister candid query method, authenticated.
pub fn query_candid_as<Input, Output>(
    env: &PocketIc,
    canister_id: Principal,
    sender: Principal,
    method: &str,
    input: Input,
) -> Result<Output, CallError>
where
    Input: ArgumentEncoder,
    Output: for<'a> ArgumentDecoder<'a>,
{
    with_candid(input, |bytes| {
        env.query_call(canister_id, sender, method, bytes)
    })
}

/// Call a canister candid method, authenticated.
/// The state machine executes update calls synchronously, so there is no need to poll for the result.
pub fn call_candid_as<Input, Output>(
    env: &PocketIc,
    canister_id: Principal,
    sender: Principal,
    method: &str,
    input: Input,
) -> Result<Output, CallError>
where
    Input: ArgumentEncoder,
    Output: for<'a> ArgumentDecoder<'a>,
{
    with_candid(input, |bytes| {
        env.update_call(canister_id, sender, method, bytes)
    })
}

/// Call a canister candid method, anonymous.
/// The state machine executes update calls synchronously, so there is no need to poll for the result.
pub fn call_candid<Input, Output>(
    env: &PocketIc,
    canister_id: Principal,
    method: &str,
    input: Input,
) -> Result<Output, CallError>
where
    Input: ArgumentEncoder,
    Output: for<'a> ArgumentDecoder<'a>,
{
    call_candid_as(env, canister_id, Principal::anonymous(), method, input)
}

#[derive(Clone, Copy, Debug)]
pub enum TryFromError {
    ValueOutOfRange(u64),
}

/// User-facing error codes.
///
/// The error codes are currently assigned using an HTTP-like
/// convention: the most significant digit is the corresponding reject
/// code and the rest is just a sequentially assigned two-digit
/// number.
#[derive(PartialOrd, Ord, Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorCode {
    SubnetOversubscribed = 101,
    MaxNumberOfCanistersReached = 102,
    CanisterOutputQueueFull = 201,
    IngressMessageTimeout = 202,
    CanisterQueueNotEmpty = 203,
    IngressHistoryFull = 204,
    CanisterNotFound = 301,
    CanisterMethodNotFound = 302,
    CanisterAlreadyInstalled = 303,
    CanisterWasmModuleNotFound = 304,
    InsufficientMemoryAllocation = 402,
    InsufficientCyclesForCreateCanister = 403,
    SubnetNotFound = 404,
    CanisterNotHostedBySubnet = 405,
    CanisterOutOfCycles = 501,
    CanisterTrapped = 502,
    CanisterCalledTrap = 503,
    CanisterContractViolation = 504,
    CanisterInvalidWasm = 505,
    CanisterDidNotReply = 506,
    CanisterOutOfMemory = 507,
    CanisterStopped = 508,
    CanisterStopping = 509,
    CanisterNotStopped = 510,
    CanisterStoppingCancelled = 511,
    CanisterInvalidController = 512,
    CanisterFunctionNotFound = 513,
    CanisterNonEmpty = 514,
    CertifiedStateUnavailable = 515,
    CanisterRejectedMessage = 516,
    QueryCallGraphLoopDetected = 517,
    UnknownManagementMessage = 518,
    InvalidManagementPayload = 519,
    InsufficientCyclesInCall = 520,
    CanisterWasmEngineError = 521,
    CanisterInstructionLimitExceeded = 522,
    CanisterInstallCodeRateLimited = 523,
    CanisterMemoryAccessLimitExceeded = 524,
    QueryCallGraphTooDeep = 525,
    QueryCallGraphTotalInstructionLimitExceeded = 526,
    CompositeQueryCalledInReplicatedMode = 527,
    QueryTimeLimitExceeded = 528,
    QueryCallGraphInternal = 529,
    InsufficientCyclesInComputeAllocation = 530,
    InsufficientCyclesInMemoryAllocation = 531,
    InsufficientCyclesInMemoryGrow = 532,
    ReservedCyclesLimitExceededInMemoryAllocation = 533,
    ReservedCyclesLimitExceededInMemoryGrow = 534,
}

impl TryFrom<u64> for ErrorCode {
    type Error = TryFromError;
    fn try_from(err: u64) -> Result<ErrorCode, Self::Error> {
        match err {
            101 => Ok(ErrorCode::SubnetOversubscribed),
            102 => Ok(ErrorCode::MaxNumberOfCanistersReached),
            201 => Ok(ErrorCode::CanisterOutputQueueFull),
            202 => Ok(ErrorCode::IngressMessageTimeout),
            203 => Ok(ErrorCode::CanisterQueueNotEmpty),
            204 => Ok(ErrorCode::IngressHistoryFull),
            301 => Ok(ErrorCode::CanisterNotFound),
            302 => Ok(ErrorCode::CanisterMethodNotFound),
            303 => Ok(ErrorCode::CanisterAlreadyInstalled),
            304 => Ok(ErrorCode::CanisterWasmModuleNotFound),
            402 => Ok(ErrorCode::InsufficientMemoryAllocation),
            403 => Ok(ErrorCode::InsufficientCyclesForCreateCanister),
            404 => Ok(ErrorCode::SubnetNotFound),
            405 => Ok(ErrorCode::CanisterNotHostedBySubnet),
            501 => Ok(ErrorCode::CanisterOutOfCycles),
            502 => Ok(ErrorCode::CanisterTrapped),
            503 => Ok(ErrorCode::CanisterCalledTrap),
            504 => Ok(ErrorCode::CanisterContractViolation),
            505 => Ok(ErrorCode::CanisterInvalidWasm),
            506 => Ok(ErrorCode::CanisterDidNotReply),
            507 => Ok(ErrorCode::CanisterOutOfMemory),
            508 => Ok(ErrorCode::CanisterStopped),
            509 => Ok(ErrorCode::CanisterStopping),
            510 => Ok(ErrorCode::CanisterNotStopped),
            511 => Ok(ErrorCode::CanisterStoppingCancelled),
            512 => Ok(ErrorCode::CanisterInvalidController),
            513 => Ok(ErrorCode::CanisterFunctionNotFound),
            514 => Ok(ErrorCode::CanisterNonEmpty),
            515 => Ok(ErrorCode::CertifiedStateUnavailable),
            516 => Ok(ErrorCode::CanisterRejectedMessage),
            517 => Ok(ErrorCode::QueryCallGraphLoopDetected),
            518 => Ok(ErrorCode::UnknownManagementMessage),
            519 => Ok(ErrorCode::InvalidManagementPayload),
            520 => Ok(ErrorCode::InsufficientCyclesInCall),
            521 => Ok(ErrorCode::CanisterWasmEngineError),
            522 => Ok(ErrorCode::CanisterInstructionLimitExceeded),
            523 => Ok(ErrorCode::CanisterInstallCodeRateLimited),
            524 => Ok(ErrorCode::CanisterMemoryAccessLimitExceeded),
            525 => Ok(ErrorCode::QueryCallGraphTooDeep),
            526 => Ok(ErrorCode::QueryCallGraphTotalInstructionLimitExceeded),
            527 => Ok(ErrorCode::CompositeQueryCalledInReplicatedMode),
            528 => Ok(ErrorCode::QueryTimeLimitExceeded),
            529 => Ok(ErrorCode::QueryCallGraphInternal),
            530 => Ok(ErrorCode::InsufficientCyclesInComputeAllocation),
            531 => Ok(ErrorCode::InsufficientCyclesInMemoryAllocation),
            532 => Ok(ErrorCode::InsufficientCyclesInMemoryGrow),
            533 => Ok(ErrorCode::ReservedCyclesLimitExceededInMemoryAllocation),
            534 => Ok(ErrorCode::ReservedCyclesLimitExceededInMemoryGrow),
            _ => Err(TryFromError::ValueOutOfRange(err)),
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // E.g. "IC0301"
        write!(f, "IC{:04}", *self as i32)
    }
}

/// The error that is sent back to users of IC if something goes
/// wrong. It's designed to be copyable and serializable so that we
/// can persist it in ingress history.
#[derive(PartialOrd, Ord, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UserError {
    pub code: ErrorCode,
    pub description: String,
}

impl fmt::Display for UserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // E.g. "IC0301: Canister 42 not found"
        write!(f, "{}: {}", self.code, self.description)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum CallError {
    Reject(String),
    UserError(UserError),
}

/// This struct describes the different types that executing a Wasm function in
/// a canister can produce
#[derive(PartialOrd, Ord, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WasmResult {
    /// Raw response, returned in a "happy" case
    Reply(#[serde(with = "serde_bytes")] Vec<u8>),
    /// Returned with an error message when the canister decides to reject the
    /// message
    Reject(String),
}

/// A helper function that we use to implement both [`call_candid`] and
/// [`query_candid`].
pub fn with_candid<Input, Output>(
    input: Input,
    f: impl FnOnce(Vec<u8>) -> Result<WasmResult, UserError>,
) -> Result<Output, CallError>
where
    Input: ArgumentEncoder,
    Output: for<'a> ArgumentDecoder<'a>,
{
    let in_bytes = encode_args(input).expect("failed to encode args");
    match f(in_bytes) {
        Ok(WasmResult::Reply(out_bytes)) => Ok(decode_args(&out_bytes).unwrap_or_else(|e| {
            panic!(
                "Failed to decode response as candid type {}:\nerror: {}\nbytes: {:?}\nutf8: {}",
                std::any::type_name::<Output>(),
                e,
                out_bytes,
                String::from_utf8_lossy(&out_bytes),
            )
        })),
        Ok(WasmResult::Reject(message)) => Err(CallError::Reject(message)),
        Err(user_error) => Err(CallError::UserError(user_error)),
    }
}

// ===================================

// By default, serde serializes Vec<u8> to a list of numbers, which is inefficient.
// This enables serializing Vec<u8> to a compact base64 representation.
#[allow(deprecated)]
pub mod base64 {
    use serde::{Deserialize, Serialize};
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Vec<u8>, s: S) -> Result<S::Ok, S::Error> {
        let base64 = base64::encode(v);
        String::serialize(&base64, s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let base64 = String::deserialize(d)?;
        base64::decode(base64.as_bytes()).map_err(|e| serde::de::Error::custom(e))
    }
}

/// Attempt to start a new PocketIC server if it's not already running.
pub fn start_or_reuse_server() -> Url {
    // Use the parent process ID to find the PocketIC server port for this `cargo test` run.
    let bin_path = std::env::var_os("POCKET_IC_BIN").expect("Missing PocketIC binary");
    let parent_pid = std::os::unix::process::parent_id();
    Command::new(PathBuf::from(bin_path))
        .arg("--pid")
        .arg(parent_pid.to_string())
        .spawn()
        .expect("Failed to start PocketIC binary");

    let port_file_path = std::env::temp_dir().join(format!("pocket_ic_{}.port", parent_pid));
    let ready_file_path = std::env::temp_dir().join(format!("pocket_ic_{}.ready", parent_pid));
    let start = Instant::now();
    loop {
        match ready_file_path.try_exists() {
            Ok(true) => {
                let port_string = std::fs::read_to_string(port_file_path)
                    .expect("Failed to read port from port file");
                let port: u16 = port_string.parse().expect("Failed to parse port to number");
                return Url::parse(&format!("http://{}:{}/", LOCALHOST, port)).unwrap();
            }
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
        if start.elapsed() > Duration::from_secs(5) {
            panic!("Failed to start PocketIC service in time");
        }
    }
}
