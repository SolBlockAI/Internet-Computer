use super::*;

use std::time::SystemTime;

use anyhow::Error;

use ic_crypto_test_utils_keys::public_keys::valid_tls_certificate_and_validation_time;
use rustls::{Certificate, ServerName};

use crate::{
    core::Run,
    snapshot::{test::create_fake_registry_client, Runner},
};

// CN = s52il-lowsg-eip4y-pt5lv-sbdpb-vg4gg-4iasu-egajp-yluji-znfz3-2qe
const TEST_CERTIFICATE: &str = "3082015530820107a00302010202136abf05c1260364e09ad5f4ad0e9cb90a6e0edb300506032b6570304a3148304606035504030c3f733532696c2d6c6f7773672d\
                                65697034792d7074356c762d73626470622d76673467672d34696173752d6567616a702d796c756a692d7a6e667a332d3271653020170d3232313131343135303230\
                                345a180f39393939313233313233353935395a304a3148304606035504030c3f733532696c2d6c6f7773672d65697034792d7074356c762d73626470622d76673467\
                                672d34696173752d6567616a702d796c756a692d7a6e667a332d327165302a300506032b65700321002b5c5af2776114a400d71995cf9cdb72ca1a26b59b875a3d70\
                                c79bf48b5f210b300506032b6570034100f3ded920aa535295c69fd97c8da2d73ce525370456cdaacc4863b25e19b0d2af1961454ac5ff9a9e182ea54034ceed0dd0\
                                2a7bd9421ae1f844c894544bca9602";

fn test_certificate() -> Vec<u8> {
    hex::decode(TEST_CERTIFICATE).unwrap()
}

fn check_certificate_verification(
    tls_verifier: &TlsVerifier,
    name: &str,
    der: Vec<u8>,
) -> Result<(), Error> {
    let crt = Certificate(der);
    let intermediates: Vec<Certificate> = vec![];
    let server_name = ServerName::try_from(name).unwrap();
    let scts: Vec<&[u8]> = vec![];
    let ocsp_response: Vec<u8> = vec![];

    tls_verifier.verify_server_cert(
        &crt,
        intermediates.as_slice(),
        &server_name,
        &mut scts.into_iter(),
        ocsp_response.as_slice(),
        SystemTime::now(),
    )?;

    Ok(())
}

#[tokio::test]
async fn test_verify_tls_certificate() -> Result<(), Error> {
    let rt = Arc::new(ArcSwapOption::empty());
    let reg = Arc::new(create_fake_registry_client(4));
    let mut runner = Runner::new(Arc::clone(&rt), reg);
    let helper = TlsVerifier::new(Arc::clone(&rt));
    runner.run().await?;

    let rt = rt.load_full().unwrap();

    for sn in rt.subnets.iter() {
        let node_name = sn.nodes[0].id.to_string();

        check_certificate_verification(
            &helper,
            node_name.as_str(),
            valid_tls_certificate_and_validation_time()
                .0
                .certificate_der,
        )?;

        // Check with different cert -> should fail
        let r = check_certificate_verification(&helper, node_name.as_str(), test_certificate());
        assert!(matches!(r, Err(_)));
    }

    Ok(())
}
