use ic_cycles_account_manager::{
    CRITICAL_ERROR_EXECUTION_CYCLES_REFUND, CRITICAL_ERROR_RESPONSE_CYCLES_REFUND,
};
use ic_error_types::ErrorCode;
use ic_ic00_types as ic00;
use ic_logger::{error, ReplicaLogger};
use ic_metrics::buckets::decimal_buckets;
use ic_metrics::MetricsRegistry;
use ic_replicated_state::metadata_state::subnet_call_context_manager::InstallCodeCallId;
use ic_types::CanisterId;
use prometheus::{HistogramVec, IntCounter};
use std::str::FromStr;

pub const FINISHED_OUTCOME_LABEL: &str = "finished";
pub const SUBMITTED_OUTCOME_LABEL: &str = "submitted";
pub const ERROR_OUTCOME_LABEL: &str = "error";
pub const SUCCESS_STATUS_LABEL: &str = "success";

pub const CRITICAL_ERROR_CALL_ID_WITHOUT_INSTALL_CODE_CALL: &str =
    "execution_environment_call_id_without_install_code_call";

/// Metrics used to monitor the performance of the execution environment.
pub(crate) struct ExecutionEnvironmentMetrics {
    subnet_messages: HistogramVec,
    pub executions_aborted: IntCounter,

    /// Critical error for responses above the maximum allowed size.
    response_cycles_refund_error: IntCounter,
    /// Critical error for executions above the maximum allowed size.
    execution_cycles_refund_error: IntCounter,
    /// Critical error for call ID and no matching install code call.
    call_id_without_install_code_call: IntCounter,
}
impl ExecutionEnvironmentMetrics {
    pub fn new(metrics_registry: &MetricsRegistry) -> Self {
        Self {
            subnet_messages: metrics_registry.histogram_vec(
                "execution_subnet_message_duration_seconds",
                "Duration of a subnet message execution, in seconds.",
                // Instruction limit for `install_code` would allow for about 100s execution, so
                // ensure we include at least until that bucket value.
                // Buckets: 1ms, 2ms, 5ms, ..., 100s, 200s, 500s
                decimal_buckets(-3, 2),
                // The `outcome` label is deprecated and should be replaced by `status` eventually.
                &["method_name", "outcome", "status"],
            ),
            executions_aborted: metrics_registry
                .int_counter("executions_aborted", "Total number of aborted executios"),
            response_cycles_refund_error: metrics_registry
                .error_counter(CRITICAL_ERROR_RESPONSE_CYCLES_REFUND),
            execution_cycles_refund_error: metrics_registry
                .error_counter(CRITICAL_ERROR_EXECUTION_CYCLES_REFUND),
            call_id_without_install_code_call: metrics_registry
                .error_counter(CRITICAL_ERROR_CALL_ID_WITHOUT_INSTALL_CODE_CALL),
        }
    }

    /// Observe the duration and count of subnet messages.
    ///
    /// The observation is divided by the name of the method as well as by the
    /// "outcome" (i.e. whether or not execution succeeded).
    ///
    /// Example 1: A successful call to ic00::create_canister is observed as:
    /// subnet_message({
    ///     "method_name": "ic00_create_canister",
    ///     "outcome": "success",
    ///     "status": "success",
    /// })
    ///
    /// Example 2: An unsuccessful call to ic00::install_code is observed as:
    /// subnet_message({
    ///     "method_name": "ic00_install_code",
    ///     "outcome": "error",
    ///     "status": "CanisterContractViolation",
    /// })
    ///
    /// Example 3: A call to a non-existing method is observed as:
    /// subnet_message({
    ///     "method_name": "unknown_method",
    ///     "outcome": "error",
    ///     "status": "CanisterMethodNotFound",
    /// })
    pub fn observe_subnet_message<T>(
        &self,
        method_name: &str,
        duration: f64,
        res: &Result<T, ErrorCode>,
    ) {
        let (outcome_label, status_label) = match res {
            Ok(_) => (FINISHED_OUTCOME_LABEL.into(), SUCCESS_STATUS_LABEL.into()),
            Err(err_code) => (ERROR_OUTCOME_LABEL.into(), format!("{:?}", err_code)),
        };

        self.observe_message_with_label(method_name, duration, outcome_label, status_label)
    }

    /// Helper function to observe the duration and count of subnet messages.
    pub(crate) fn observe_message_with_label(
        &self,
        method_name: &str,
        duration: f64,
        outcome_label: String,
        status_label: String,
    ) {
        let method_name_label = if let Ok(method_name) = ic00::Method::from_str(method_name) {
            format!("ic00_{}", method_name)
        } else {
            String::from("unknown_method")
        };

        self.subnet_messages
            .with_label_values(&[&method_name_label, &outcome_label, &status_label])
            .observe(duration);
    }

    pub fn response_cycles_refund_error_counter(&self) -> &IntCounter {
        &self.response_cycles_refund_error
    }

    pub fn execution_cycles_refund_error_counter(&self) -> &IntCounter {
        &self.execution_cycles_refund_error
    }

    pub fn observe_call_id_without_install_code_call_error_counter(
        &self,
        log: &ReplicaLogger,
        call_id: InstallCodeCallId,
        canister_id: CanisterId,
    ) {
        self.call_id_without_install_code_call.inc();
        error!(
            log,
            "[EXC-BUG] Could not find any install code call for the specified call ID {} for canister {} after the execution of install code",
            call_id,
            canister_id,
        );
    }
}
