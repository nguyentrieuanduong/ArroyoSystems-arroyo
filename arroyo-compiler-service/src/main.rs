use std::process::{exit, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{io, path::PathBuf, str::FromStr, sync::Arc};

use anyhow::bail;
use arroyo_rpc::grpc::{
    compiler_grpc_server::{CompilerGrpc, CompilerGrpcServer},
    CompileQueryReq, CompileQueryResp,
};

use arroyo_server_common::start_admin_server;
use arroyo_storage::StorageProvider;
use arroyo_types::{grpc_port, ports, ARTIFACT_URL_ENV};
use prost::Message;
use tokio::sync::broadcast;
use tokio::{process::Command, sync::Mutex};
use tonic::{transport::Server, Request, Response, Status};
use tracing::error;
use tracing::info;

pub fn to_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

pub fn from_millis(ts: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ts)
}

#[tokio::main]
pub async fn main() {
    let _guard = arroyo_server_common::init_logging("compiler-service");

    let build_dir = std::env::var("BUILD_DIR").unwrap_or("build_dir".to_string());
    let debug = std::env::var("DEBUG").is_ok();

    let artifact_url = std::env::var(ARTIFACT_URL_ENV)
        .unwrap_or_else(|_| panic!("{} must be set", ARTIFACT_URL_ENV));

    let storage =
        StorageProvider::for_url(&artifact_url).expect("unable to construct storage provider");

    let last_used = Arc::new(AtomicU64::new(to_millis(SystemTime::now())));

    let service = CompileService {
        build_dir: PathBuf::from_str(&build_dir).unwrap(),
        lock: Arc::new(Mutex::new(())),
        last_used: last_used.clone(),
        storage,
        debug,
    };

    let args = std::env::args().collect::<Vec<_>>();
    match args.get(1) {
        Some(arg) if arg == "start" => {
            start_service(service).await;
        }
        Some(arg) if arg == "compile" => {
            let path = args
                .get(2)
                .expect("Usage: ./compiler_service compile <query-req-path>");

            let query = service
                .storage
                .get(path)
                .await
                .expect("Failed to read query from storage");

            let query = CompileQueryReq::decode(&*query).expect("Failed to decode query request");

            let resp = service.compile(query).await.unwrap();
            println!(
                "{{\"pipeline_path\": \"{}\", \"wasm_fns_path\": \"{}\"}}",
                resp.pipeline_path, resp.wasm_fns_path
            );
        }
        _ => {
            println!("Usage: {} start|compile", args.get(0).unwrap());
        }
    }
}

pub async fn start_service(service: CompileService) {
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);

    start_admin_server("compiler", ports::COMPILER_ADMIN, shutdown_rx.resubscribe());

    let grpc = grpc_port("compiler", ports::COMPILER_GRPC);

    let addr = format!("0.0.0.0:{}", grpc).parse().unwrap();

    info!("Starting compiler service at {}", addr);
    info!(
        "artifacts will be written to {}",
        service.storage.canonical_url()
    );

    let last_used = service.last_used.clone();

    if let Ok(idle_time) =
        std::env::var("IDLE_SHUTDOWN_MS").map(|t| Duration::from_millis(u64::from_str(&t).unwrap()))
    {
        tokio::spawn(async move {
            loop {
                if from_millis(last_used.load(Ordering::Relaxed))
                    .elapsed()
                    .unwrap()
                    > idle_time
                {
                    println!("Idle time exceeded, shutting down");
                    exit(0);
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    Server::builder()
        .max_frame_size(Some((1 << 24) - 1)) // 16MB
        .add_service(CompilerGrpcServer::new(service))
        .serve(addr)
        .await
        .unwrap();

    shutdown_tx.send(0).unwrap();
}

pub struct CompileService {
    build_dir: PathBuf,
    lock: Arc<Mutex<()>>,
    last_used: Arc<AtomicU64>,
    storage: StorageProvider,
    debug: bool,
}

impl CompileService {
    async fn get_output(&self) -> io::Result<Output> {
        if self.debug {
            let args = if std::env::var("VERBOSE").is_ok() {
                vec!["build", "--verbose"]
            } else {
                vec!["build"]
            };

            Command::new("cargo")
                .current_dir(&self.build_dir)
                .args(&args)
                .output()
                .await
        } else {
            Command::new("cargo")
                .current_dir(&self.build_dir)
                .arg("build")
                .arg("--release")
                .output()
                .await
        }
    }

    fn pipeline_path(&self) -> &str {
        if self.debug {
            "target/debug/pipeline"
        } else {
            "target/release/pipeline"
        }
    }

    async fn compile(&self, req: CompileQueryReq) -> anyhow::Result<CompileQueryResp> {
        info!("Starting compilation for {}", req.job_id);
        let start = Instant::now();
        let build_dir = &self.build_dir;
        tokio::fs::write(build_dir.join("pipeline/src/main.rs"), &req.pipeline).await?;

        tokio::fs::write(build_dir.join("types/src/lib.rs"), &req.types).await?;

        tokio::fs::write(build_dir.join("wasm-fns/src/lib.rs"), &req.wasm_fns).await?;

        let result = self.get_output().await?;

        if !result.status.success() {
            bail!(
                "Failed to compile job: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        } else if self.debug {
            info!(
                "cargo build stderr: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }

        if !req.wasm_fns.is_empty() {
            let result = Command::new("wasm-pack")
                .arg("build")
                .current_dir(&build_dir.join("wasm-fns"))
                .output()
                .await
                .expect("wasm-pack not found -- install with `cargo install wasm-pack`");

            if !result.status.success() {
                bail!(
                    "Failed to compile wasm: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
            }
        }

        info!(
            "Finished compilation after {:.2}s",
            start.elapsed().as_secs_f32()
        );

        // TODO: replace this with the SHA of the worker code once that's available
        let id = (to_millis(SystemTime::now()) / 1000).to_string();

        let base = format!("{}/artifacts/{}", &req.job_id, id);

        {
            let pipeline = tokio::fs::read(&build_dir.join(self.pipeline_path())).await?;
            self.storage
                .put(format!("{}/pipeline", base), pipeline)
                .await?;
        }

        {
            let wasm_fns =
                tokio::fs::read(&build_dir.join("wasm-fns/pkg/wasm_fns_bg.wasm")).await?;
            self.storage
                .put(format!("{}/wasm_fns_bg.wasm", base), wasm_fns)
                .await?;
        }

        let full_path = format!("{}/{}", self.storage.canonical_url(), base);

        Ok(CompileQueryResp {
            pipeline_path: format!("{}/pipeline", full_path),
            wasm_fns_path: format!("{}/wasm_fns_bg.wasm", full_path),
        })
    }
}

#[tonic::async_trait]
impl CompilerGrpc for CompileService {
    async fn compile_query(
        &self,
        request: Request<CompileQueryReq>,
    ) -> Result<Response<CompileQueryResp>, Status> {
        self.last_used
            .store(to_millis(SystemTime::now()), Ordering::Relaxed);

        // only allow one request to be active at a given time
        let _guard = self.lock.lock().await;

        let req = request.into_inner();

        self.compile(req).await.map(Response::new).map_err(|e| {
            error!("Failed to compile: {:?}", e);
            Status::internal(e.to_string())
        })
    }
}
