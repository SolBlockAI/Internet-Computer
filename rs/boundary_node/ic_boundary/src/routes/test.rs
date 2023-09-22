use super::*;

use crate::persist::test::node;
use ic_types::messages::{Blob, HttpQueryContent, HttpRequestEnvelope, HttpUserQuery};

struct ProxyRouter {
    node: Node,
    root_key: Vec<u8>,
}

#[async_trait]
impl Proxy for ProxyRouter {
    async fn proxy(
        &self,
        _request_type: RequestType,
        _request: Request<Body>,
        _node: Node,
        _canister_id: Principal,
    ) -> Result<Response, ErrorCause> {
        Ok("foobar".into_response())
    }
}

#[async_trait]
impl Lookup for ProxyRouter {
    async fn lookup(&self, _: &Principal) -> Result<Node, ErrorCause> {
        Ok(self.node.clone())
    }
}

#[async_trait]
impl RootKey for ProxyRouter {
    async fn root_key(&self) -> Vec<u8> {
        self.root_key.clone()
    }
}

#[async_trait]
impl Health for ProxyRouter {
    async fn health(&self) -> ReplicaHealthStatus {
        ReplicaHealthStatus::Healthy
    }
}

#[tokio::test]
async fn test_status() -> Result<(), Error> {
    let node = node(0, Principal::from_text("f7crg-kabae").unwrap());
    let root_key = vec![8, 6, 7, 5, 3, 0, 9];

    let proxy_router = Arc::new(ProxyRouter {
        node,
        root_key: root_key.clone(),
    });

    let (rk, h) = (
        proxy_router.clone() as Arc<dyn RootKey>,
        proxy_router.clone() as Arc<dyn Health>,
    );

    let resp: Response = status(State((rk, h))).await?.into_response();
    assert_eq!(resp.status(), StatusCode::OK);

    let (_parts, body) = resp.into_parts();
    let body = hyper::body::to_bytes(body).await.unwrap().to_vec();

    let health: HttpStatusResponse = serde_cbor::from_slice(&body)?;
    assert_eq!(
        health.replica_health_status,
        Some(ReplicaHealthStatus::Healthy)
    );
    assert_eq!(health.root_key.as_deref(), Some(&root_key),);

    Ok(())
}

#[tokio::test]
async fn test_query() -> Result<(), Error> {
    let node = node(0, Principal::from_text("f7crg-kabae").unwrap());
    let root_key = vec![8, 6, 7, 5, 3, 0, 9];
    let state = ProxyRouter {
        node: node.clone(),
        root_key,
    };

    let sender = Principal::from_text("sqjm4-qahae-aq").unwrap();
    let canister_id = Principal::from_text("sxiki-5ygae-aq").unwrap();

    let content = HttpQueryContent::Query {
        query: HttpUserQuery {
            canister_id: Blob(canister_id.as_slice().to_vec()),
            method_name: "foobar".to_string(),
            arg: Blob(vec![]),
            sender: Blob(sender.as_slice().to_vec()),
            nonce: None,
            ingress_expiry: 1234,
        },
    };

    let envelope = HttpRequestEnvelope::<HttpQueryContent> {
        content,
        sender_delegation: None,
        sender_pubkey: None,
        sender_sig: None,
    };

    let body = serde_cbor::to_vec(&envelope).unwrap();

    let mut ctx = RequestContext::default();
    parse_body(&mut ctx, &body)?;
    ctx.canister_id = Some(canister_id);
    ctx.node = Some(node);

    let request = Request::builder().body(Body::from(body)).unwrap();

    let resp = query(State(Arc::new(state)), Extension(ctx), request)
        .await
        .unwrap()
        .into_response();

    assert_eq!(resp.status(), StatusCode::OK);

    let (_parts, body) = resp.into_parts();
    let body = hyper::body::to_bytes(body).await.unwrap().to_vec();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(body, "foobar");

    Ok(())
}
