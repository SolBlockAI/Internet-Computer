use ic_agent::{export::Principal, Agent};

use anyhow::Error;
use async_trait::async_trait;
use opentelemetry::{baggage::BaggageExt, metrics::Meter, Context};
use tokio::time::Instant;

use opentelemetry::{
    metrics::{Counter, Histogram},
    KeyValue,
};
use tracing::info;

use crate::{Create, Delete, Install, Load, Probe, Routes, Run, Stop, TestContext};

#[derive(Clone)]
pub struct MetricParams {
    pub counter: Counter<u64>,
    pub recorder: Histogram<f64>,
}

impl MetricParams {
    pub fn new(meter: &Meter, namespace: &str, name: &str) -> Self {
        Self {
            counter: meter
                .u64_counter(format!("{namespace}.{name}"))
                .with_description(format!("Counts occurrences of {namespace}.{name} calls"))
                .init(),
            recorder: meter
                .f64_histogram(format!("{namespace}.{name}.duration_sec"))
                .with_description(format!(
                    "Records the duration of {namespace}.{name} calls in sec"
                ))
                .init(),
        }
    }
}

#[derive(Clone)]
pub struct WithMetrics<T>(pub T, pub MetricParams);

#[async_trait]
impl<T: Load> Load for WithMetrics<T> {
    async fn load(&self) -> Result<Routes, Error> {
        let start_time = Instant::now();

        let out = self.0.load().await;

        let status = if out.is_ok() { "ok" } else { "fail" };
        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[KeyValue::new("status", status)];

        let MetricParams { counter, recorder } = &self.1;
        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = "load", status, duration, error = ?out.as_ref().err());

        out
    }
}

#[async_trait]
impl<T: Create> Create for WithMetrics<T> {
    async fn create(&self, agent: &Agent, wallet_id: &str) -> Result<Principal, Error> {
        let start_time = Instant::now();

        let out = self.0.create(agent, wallet_id).await;

        let status = if out.is_ok() { "ok" } else { "fail" };
        let duration = start_time.elapsed().as_secs_f64();

        let ctx = Context::current();
        let ctx = ctx.baggage();

        let labels = &[
            KeyValue::new("action", "create"),
            KeyValue::new("subnet_id", ctx.get("subnet_id").unwrap().to_string()),
            KeyValue::new("node_id", ctx.get("node_id").unwrap().to_string()),
            KeyValue::new("socket_addr", ctx.get("socket_addr").unwrap().to_string()),
            KeyValue::new("status", status),
            KeyValue::new("wallet", wallet_id.to_string()),
        ];

        let MetricParams { counter, recorder } = &self.1;
        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(
            action = "create",
            subnet_id = ctx.get("subnet_id").unwrap().to_string().as_str(),
            node_id = ctx.get("node_id").unwrap().to_string().as_str(),
            socket_addr = ctx.get("socket_addr").unwrap().to_string().as_str(),
            wallet = wallet_id.to_string().as_str(),
            status,
            duration,
            error = ?out.as_ref().err(),
        );

        out
    }
}

#[async_trait]
impl<T: Install> Install for WithMetrics<T> {
    async fn install(
        &self,
        agent: &Agent,
        wallet_id: &str,
        canister_id: Principal,
    ) -> Result<(), Error> {
        let start_time = Instant::now();

        let out = self.0.install(agent, wallet_id, canister_id).await;

        let status = if out.is_ok() { "ok" } else { "fail" };
        let duration = start_time.elapsed().as_secs_f64();

        let ctx = Context::current();
        let ctx = ctx.baggage();

        let labels = &[
            KeyValue::new("action", "install"),
            KeyValue::new("subnet_id", ctx.get("subnet_id").unwrap().to_string()),
            KeyValue::new("node_id", ctx.get("node_id").unwrap().to_string()),
            KeyValue::new("socket_addr", ctx.get("socket_addr").unwrap().to_string()),
            KeyValue::new("status", status),
            KeyValue::new("wallet", wallet_id.to_string()),
        ];

        let MetricParams { counter, recorder } = &self.1;
        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(
            action = "install",
            subnet_id = ctx.get("subnet_id").unwrap().to_string().as_str(),
            node_id = ctx.get("node_id").unwrap().to_string().as_str(),
            socket_addr = ctx.get("socket_addr").unwrap().to_string().as_str(),
            wallet = wallet_id.to_string().as_str(),
            canister = canister_id.to_string().as_str(),
            status,
            duration,
            error = ?out.as_ref().err(),
        );

        out
    }
}

#[async_trait]
impl<T: Probe> Probe for WithMetrics<T> {
    async fn probe(&self, agent: &Agent, canister_id: Principal) -> Result<(), Error> {
        let start_time = Instant::now();

        let out = self.0.probe(agent, canister_id).await;

        let status = if out.is_ok() { "ok" } else { "fail" };
        let duration = start_time.elapsed().as_secs_f64();

        let ctx = Context::current();
        let ctx = ctx.baggage();

        let labels = &[
            KeyValue::new("action", "probe"),
            KeyValue::new("subnet_id", ctx.get("subnet_id").unwrap().to_string()),
            KeyValue::new("node_id", ctx.get("node_id").unwrap().to_string()),
            KeyValue::new("socket_addr", ctx.get("socket_addr").unwrap().to_string()),
            KeyValue::new("status", status),
        ];

        let MetricParams { counter, recorder } = &self.1;
        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(
            action = "probe",
            subnet_id = ctx.get("subnet_id").unwrap().to_string().as_str(),
            node_id = ctx.get("node_id").unwrap().to_string().as_str(),
            socket_addr = ctx.get("socket_addr").unwrap().to_string().as_str(),
            canister = canister_id.to_string().as_str(),
            status,
            duration,
            error = ?out.as_ref().err(),
        );

        out
    }
}

#[async_trait]
impl<T: Stop> Stop for WithMetrics<T> {
    async fn stop(
        &self,
        agent: &Agent,
        wallet_id: &str,
        canister_id: Principal,
    ) -> Result<(), Error> {
        let start_time = Instant::now();

        let out = self.0.stop(agent, wallet_id, canister_id).await;

        let status = if out.is_ok() { "ok" } else { "fail" };
        let duration = start_time.elapsed().as_secs_f64();

        let ctx = Context::current();
        let ctx = ctx.baggage();

        let labels = &[
            KeyValue::new("action", "stop"),
            KeyValue::new("subnet_id", ctx.get("subnet_id").unwrap().to_string()),
            KeyValue::new("node_id", ctx.get("node_id").unwrap().to_string()),
            KeyValue::new("socket_addr", ctx.get("socket_addr").unwrap().to_string()),
            KeyValue::new("status", status),
            KeyValue::new("wallet", wallet_id.to_string()),
        ];

        let MetricParams { counter, recorder } = &self.1;
        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(
            action = "stop",
            subnet_id = ctx.get("subnet_id").unwrap().to_string().as_str(),
            node_id = ctx.get("node_id").unwrap().to_string().as_str(),
            socket_addr = ctx.get("socket_addr").unwrap().to_string().as_str(),
            wallet = wallet_id.to_string().as_str(),
            canister = canister_id.to_string().as_str(),
            status,
            duration,
            error = ?out.as_ref().err(),
        );

        out
    }
}

#[async_trait]
impl<T: Delete> Delete for WithMetrics<T> {
    async fn delete(
        &self,
        agent: &Agent,
        wallet_id: &str,
        canister_id: Principal,
    ) -> Result<(), Error> {
        let start_time = Instant::now();

        let out = self.0.delete(agent, wallet_id, canister_id).await;

        let status = if out.is_ok() { "ok" } else { "fail" };
        let duration = start_time.elapsed().as_secs_f64();

        let ctx = Context::current();
        let ctx = ctx.baggage();

        let labels = &[
            KeyValue::new("action", "delete"),
            KeyValue::new("subnet_id", ctx.get("subnet_id").unwrap().to_string()),
            KeyValue::new("node_id", ctx.get("node_id").unwrap().to_string()),
            KeyValue::new("socket_addr", ctx.get("socket_addr").unwrap().to_string()),
            KeyValue::new("status", status),
            KeyValue::new("wallet", wallet_id.to_string()),
        ];

        let MetricParams { counter, recorder } = &self.1;
        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(
            action = "delete",
            subnet_id = ctx.get("subnet_id").unwrap().to_string().as_str(),
            node_id = ctx.get("node_id").unwrap().to_string().as_str(),
            socket_addr = ctx.get("socket_addr").unwrap().to_string().as_str(),
            wallet = wallet_id.to_string().as_str(),
            canister = canister_id.to_string().as_str(),
            status,
            duration,
            error = ?out.as_ref().err(),
        );

        out
    }
}

#[async_trait]
impl<T: Run> Run for WithMetrics<T> {
    async fn run(&mut self, context: &TestContext) -> Result<(), Error> {
        let start_time = Instant::now();

        let out = self.0.run(context).await;

        let status = if out.is_ok() { "ok" } else { "fail" };
        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[KeyValue::new("status", status)];

        let MetricParams { counter, recorder } = &self.1;
        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = "run", status, duration, error = ?out.as_ref().err());

        out
    }
}
