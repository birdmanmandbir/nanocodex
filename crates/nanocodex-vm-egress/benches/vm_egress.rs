use std::{hint::black_box, sync::Arc, time::Duration};

use async_trait::async_trait;
use axum::{
    Router,
    extract::Request,
    http::{StatusCode, header::AUTHORIZATION},
    routing::any,
};
use criterion::{Criterion, criterion_group, criterion_main};
use mpp::{MppError, PaymentChallenge, PaymentCredential, client::PaymentProvider};
use nanocodex_vm_egress::{
    EgressContext, SecretDelivery, SecretError, SecretGuestConfig, SecretManager, SecretRef,
    SecretRequestRule, SecretSpec, StaticSecretPolicy, UnmatchedEgress, VmEgress,
};

const SECRET: &str = "benchmark-host-only";

#[derive(Clone, Copy)]
struct NoPayments;

impl PaymentProvider for NoPayments {
    fn supports(&self, _method: &str, _intent: &str) -> bool {
        false
    }

    async fn pay(&self, _challenge: &PaymentChallenge) -> Result<PaymentCredential, MppError> {
        Err(MppError::UnsupportedPaymentMethod("benchmark".to_owned()))
    }
}

struct BenchmarkSecretManager;

#[async_trait]
impl SecretManager for BenchmarkSecretManager {
    async fn resolve(&self, _reference: &SecretRef) -> Result<String, SecretError> {
        Ok(SECRET.to_owned())
    }
}

async fn origin(request: Request) -> (StatusCode, &'static str) {
    if request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        == Some("Bearer benchmark-host-only")
    {
        (StatusCode::OK, "secret")
    } else {
        (StatusCode::OK, "plain")
    }
}

async fn request(client: &reqwest::Client, url: &str, expected: &str) {
    let response = client.get(url).send().await.expect("benchmark request");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.expect("benchmark response body");
    assert_eq!(body, expected);
    black_box(body);
}

#[allow(
    clippy::too_many_lines,
    reason = "the benchmark keeps one shared fixture and three directly comparable paths together"
)]
fn benchmark_vm_egress(criterion: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("benchmark runtime");
    let (origin_task, url, plain_egress, secret_egress, direct, plain, secret) =
        runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind benchmark origin");
            let url = format!(
                "http://{}/benchmark",
                listener.local_addr().expect("benchmark origin address")
            );
            let origin_task = tokio::spawn(async move {
                axum::serve(listener, Router::new().fallback(any(origin)))
                    .await
                    .expect("serve benchmark origin");
            });

            let plain_egress = VmEgress::builder(NoPayments)
                .spawn()
                .await
                .expect("start plain egress");
            let route = SecretSpec::builder(
                "benchmark",
                SecretRef::new("benchmark", "token"),
                url.trim_end_matches("/benchmark"),
                SecretDelivery::inject_header("authorization", "Bearer "),
                SecretGuestConfig::new("BENCHMARK_BASE_URL"),
            )
            .rule(SecretRequestRule::new().path_prefix("/benchmark"))
            .build()
            .expect("build benchmark route");
            let secret_egress = VmEgress::builder(NoPayments)
                .unmatched_egress(UnmatchedEgress::Deny)
                .secrets(
                    EgressContext::new("benchmark-agent", "benchmark-principal"),
                    Arc::new(StaticSecretPolicy::new([route])),
                    Arc::new(BenchmarkSecretManager),
                )
                .spawn()
                .await
                .expect("start secret egress");

            let direct = reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("direct benchmark client");
            let plain = reqwest::Client::builder()
                .proxy(
                    reqwest::Proxy::all(plain_egress.proxy_url()).expect("plain benchmark proxy"),
                )
                .build()
                .expect("plain benchmark client");
            let secret = reqwest::Client::builder()
                .proxy(
                    reqwest::Proxy::all(secret_egress.proxy_url()).expect("secret benchmark proxy"),
                )
                .build()
                .expect("secret benchmark client");
            request(&direct, &url, "plain").await;
            request(&plain, &url, "plain").await;
            request(&secret, &url, "secret").await;
            (
                origin_task,
                url,
                plain_egress,
                secret_egress,
                direct,
                plain,
                secret,
            )
        });

    let mut group = criterion.benchmark_group("vm_egress_loopback");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(2));
    group.bench_function("direct_origin_round_trip", |bencher| {
        bencher
            .to_async(&runtime)
            .iter(|| request(&direct, &url, "plain"));
    });
    group.bench_function("authenticated_mpp_proxy_round_trip", |bencher| {
        bencher
            .to_async(&runtime)
            .iter(|| request(&plain, &url, "plain"));
    });
    group.bench_function("dynamic_policy_secret_injection_round_trip", |bencher| {
        bencher
            .to_async(&runtime)
            .iter(|| request(&secret, &url, "secret"));
    });
    group.finish();

    drop((direct, plain, secret));
    runtime.block_on(async {
        plain_egress
            .shutdown()
            .await
            .expect("shutdown plain egress");
        secret_egress
            .shutdown()
            .await
            .expect("shutdown secret egress");
    });
    origin_task.abort();
}

criterion_group!(benches, benchmark_vm_egress);
criterion_main!(benches);
