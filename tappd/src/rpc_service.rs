use std::sync::Arc;

use anyhow::{bail, Context, Result};
use ra_rpc::{CallContext, RpcCall};
use ra_tls::{
    attestation::QuoteContentType,
    cert::{CaCert, CertRequest},
    kdf::derive_ecdsa_key_pair,
    qvl::quote::Report,
};
use serde_json::json;
use tappd_rpc::{
    tappd_server::{TappdRpc, TappdServer},
    worker_server::{WorkerRpc, WorkerServer},
    DeriveKeyArgs, DeriveKeyResponse, TdxQuoteArgs, TdxQuoteResponse, WorkerInfo, WorkerVersion,
};
use tdx_attest::eventlog::read_event_logs;

use crate::config::Config;

#[derive(Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    config: Config,
    ca: CaCert,
}

impl AppState {
    pub fn new(config: Config) -> Result<Self> {
        let ca = CaCert::load(&config.cert_file, &config.key_file)
            .context("Failed to load CA certificate")?;
        Ok(Self {
            inner: Arc::new(AppStateInner { config, ca }),
        })
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }
}

pub struct InternalRpcHandler {
    state: AppState,
}

impl TappdRpc for InternalRpcHandler {
    async fn derive_key(self, request: DeriveKeyArgs) -> Result<DeriveKeyResponse> {
        let derived_key =
            derive_ecdsa_key_pair(&self.state.inner.ca.key, &[request.path.as_bytes()])
                .context("Failed to derive key")?;
        let req = CertRequest::builder()
            .subject(&request.subject)
            .alt_names(&request.alt_names)
            .key(&derived_key)
            .build();
        let cert = self
            .state
            .inner
            .ca
            .sign(req)
            .context("Failed to sign certificate")?;
        Ok(DeriveKeyResponse {
            key: derived_key.serialize_pem(),
            certificate_chain: vec![cert.pem(), self.state.inner.ca.cert.pem()],
        })
    }

    async fn tdx_quote(self, request: TdxQuoteArgs) -> Result<TdxQuoteResponse> {
        let report_data = QuoteContentType::AppData
            .to_report_data_with_hash(&request.report_data, &request.hash_algorithm)?;
        let event_log = read_event_logs().context("Failed to decode event log")?;
        let event_log =
            serde_json::to_string(&event_log).context("Failed to serialize event log")?;
        let (_, quote) =
            tdx_attest::get_quote(&report_data, None).context("Failed to get quote")?;
        Ok(TdxQuoteResponse { quote, event_log })
    }

    async fn info(self) -> Result<WorkerInfo> {
        ExternalRpcHandler { state: self.state }.info().await
    }
}

impl RpcCall<AppState> for InternalRpcHandler {
    type PrpcService = TappdServer<Self>;

    fn into_prpc_service(self) -> Self::PrpcService {
        TappdServer::new(self)
    }

    fn construct(context: CallContext<'_, AppState>) -> Result<Self>
    where
        Self: Sized,
    {
        Ok(InternalRpcHandler {
            state: context.state.clone(),
        })
    }
}

pub struct ExternalRpcHandler {
    state: AppState,
}

impl ExternalRpcHandler {
    pub(crate) fn new(state: AppState) -> Self {
        Self { state }
    }
}

impl WorkerRpc for ExternalRpcHandler {
    async fn info(self) -> Result<WorkerInfo> {
        let ca = &self.state.inner.ca;
        let Some(attestation) = ca.decode_attestation().ok().flatten() else {
            return Ok(WorkerInfo::default());
        };
        let app_id = attestation
            .decode_app_id()
            .context("Failed to decode app id")?;
        let instance_id = attestation
            .decode_instance_id()
            .context("Failed to decode instance_id")?;
        let quote = attestation
            .decode_quote()
            .context("Failed to decode quote")?;
        let rootfs_hash = attestation
            .decode_rootfs_hash()
            .context("Failed to decode rootfs hash")?;
        let report = match &quote.report {
            Report::SgxEnclave(_) => bail!("SGX reports are not supported"),
            Report::TD10(tdreport10) => tdreport10,
            Report::TD15(tdreport15) => &tdreport15.base,
        };
        let event_log = &attestation.event_log;
        let mrtd = hex::encode(report.mr_td);
        let rtmr0 = hex::encode(report.rt_mr0);
        let rtmr1 = hex::encode(report.rt_mr1);
        let rtmr2 = hex::encode(report.rt_mr2);
        let rtmr3 = hex::encode(report.rt_mr3);
        let tcb_info = serde_json::to_string_pretty(&json!({
            "rootfs_hash": rootfs_hash,
            "mrtd": mrtd,
            "rtmr0": rtmr0,
            "rtmr1": rtmr1,
            "rtmr2": rtmr2,
            "rtmr3": rtmr3,
            "event_log": event_log,
        }))
        .unwrap_or_default();
        Ok(WorkerInfo {
            app_id,
            instance_id,
            app_cert: ca.pem_cert.clone(),
            tcb_info,
        })
    }

    async fn version(self) -> Result<WorkerVersion> {
        Ok(WorkerVersion {
            version: env!("CARGO_PKG_VERSION").to_string(),
        })
    }
}

impl RpcCall<AppState> for ExternalRpcHandler {
    type PrpcService = WorkerServer<Self>;

    fn into_prpc_service(self) -> Self::PrpcService {
        WorkerServer::new(self)
    }

    fn construct(context: CallContext<'_, AppState>) -> Result<Self>
    where
        Self: Sized,
    {
        Ok(ExternalRpcHandler {
            state: context.state.clone(),
        })
    }
}
