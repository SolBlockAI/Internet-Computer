use crate::CertificationVersion;
use ic_error_types::RejectCode;
use ic_test_utilities::types::{
    ids::canister_test_id,
    messages::{RequestBuilder, ResponseBuilder},
};
use ic_types::{
    messages::{CallbackId, Payload, RejectContext, RequestMetadata, RequestOrResponse},
    xnet::StreamHeader,
    Cycles, Time,
};
use std::collections::VecDeque;

pub fn stream_header(certification_version: CertificationVersion) -> StreamHeader {
    StreamHeader {
        begin: 23.into(),
        end: 25.into(),
        signals_end: 256.into(),
        reject_signals: if certification_version < CertificationVersion::V8 {
            VecDeque::new()
        } else {
            vec![10.into(), 200.into(), 250.into()].into()
        },
    }
}

pub fn request(certification_version: CertificationVersion) -> RequestOrResponse {
    RequestBuilder::new()
        .receiver(canister_test_id(1))
        .sender(canister_test_id(2))
        .sender_reply_callback(CallbackId::from(3))
        .payment(Cycles::new(4))
        .method_name("test".to_string())
        .method_payload(vec![6])
        .metadata(
            (certification_version >= CertificationVersion::V14).then_some(RequestMetadata {
                call_tree_depth: Some(1),
                call_tree_start_time: Some(Time::from_nanos_since_unix_epoch(100_000)),
                call_subtree_deadline: Some(Time::from_nanos_since_unix_epoch(100_001)),
            }),
        )
        .build()
        .into()
}

pub fn response() -> RequestOrResponse {
    ResponseBuilder::new()
        .originator(canister_test_id(6))
        .respondent(canister_test_id(5))
        .originator_reply_callback(CallbackId::from(4))
        .refund(Cycles::new(3))
        .response_payload(Payload::Data(vec![1]))
        .build()
        .into()
}

pub fn reject_response() -> RequestOrResponse {
    ResponseBuilder::new()
        .originator(canister_test_id(6))
        .respondent(canister_test_id(5))
        .originator_reply_callback(CallbackId::from(4))
        .refund(Cycles::new(3))
        .response_payload(Payload::Reject(RejectContext::new(
            RejectCode::SysFatal,
            "Oops",
        )))
        .build()
        .into()
}
