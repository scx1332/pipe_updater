use chrono::{DateTime, Local};
use std::path::{Path, PathBuf};
use std::{env, fs, io, thread};

use actix_web::{get, web, App, HttpResponse, HttpServer, Responder};
use flexi_logger::*;

use human_bytes::human_bytes;
use lazy_static::lazy_static; // 1.4.0
use pipe_downloader::pipe_downloader::{PipeDownloader, PipeDownloaderOptions, ProgressContext};
use std::sync::Arc;
use std::sync::Mutex;
use structopt::StructOpt;
use tokio::task;

struct UpdateTask {
    downloader: Arc<Mutex<PipeDownloader>>,
    process_to_close: Option<String>,
    is_running: Arc<Mutex<bool>>,
}

fn update_task_main(
    process_to_close: Option<String>,
    downloader: Arc<Mutex<PipeDownloader>>,
) -> anyhow::Result<()> {
    if let Some(process_to_close) = process_to_close.as_ref() {
        match systemctl::stop(process_to_close) {
            Ok(_) => {}
            Err(e) => {
                println!("Error stopping service: {}", e);
                return Err(anyhow::anyhow!("Error stopping service: {}", e));
            }
        };
    }
    match downloader.lock().unwrap().start_download() {
        Ok(_) => {}
        Err(e) => {
            println!("Error started downloading: {}", e);
            return Err(anyhow::anyhow!("Error started downloading: {}", e));
        }
    };

    while !downloader.lock().unwrap().is_finished() {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    if let Some(process_to_close) = process_to_close.as_ref() {
        match systemctl::restart(process_to_close) {
            Ok(_) => {}
            Err(e) => {
                println!("Error starting service: {}", e);
                return Err(anyhow::anyhow!("Error starting service: {}", e));
            }
        };
    }
    Ok(())
}

impl UpdateTask {
    fn new(downloader: PipeDownloader, process_to_close: Option<String>) -> Self {
        Self {
            downloader: Arc::new(Mutex::new(downloader)),
            process_to_close,
            is_running: Arc::new(Mutex::new(false)),
        }
    }

    fn is_running(self: &Self) -> bool {
        *self.is_running.lock().unwrap()
    }

    fn get_progress(self: &Self) -> anyhow::Result<ProgressContext> {
        let progress = self.downloader.lock().unwrap().get_progress();
        Ok(progress?)
    }

    fn run(self: &mut Self) -> anyhow::Result<()> {
        if *self.is_running.lock().unwrap() {
            return Err(anyhow::anyhow!("Task is already running"));
        }
        *self.is_running.lock().unwrap() = true;
        let process_to_close = self.process_to_close.clone();
        let downloader = self.downloader.clone();
        let is_running = self.is_running.clone();

        thread::spawn(move || {
            match update_task_main(process_to_close, downloader) {
                Ok(_) => {}
                Err(e) => {
                    println!("Error running update task: {}", e);
                }
            };
            *is_running.lock().unwrap() = false;
        });
        Ok(())
    }
}

struct AppState {
    started: bool,
    lighthouse_updater: Option<UpdateTask>,
}

lazy_static! {
    static ref UPDATER_STATE: Arc<Mutex<AppState>> = Arc::new(Mutex::new(AppState {
        started: false,
        lighthouse_updater: None
    }));
}

#[derive(StructOpt, Debug)]
struct Cli {
    /// Path to write logs to
    #[structopt(long, short)]
    pub log_dir: Option<PathBuf>,
    // /// Listen address
    #[structopt(long, default_value = "/usr/bin/systemctl")]
    pub systemctl_path: PathBuf,
}
fn setup_logging(log_dir: Option<impl AsRef<Path>>) -> anyhow::Result<()> {
    let log_level = env::var("PROXY_LOG").unwrap_or_else(|_| "info".into());
    env::set_var("PROXY_LOG", &log_level);

    let mut logger = Logger::try_with_str(&log_level)?;

    if let Some(log_dir) = log_dir {
        let log_dir = log_dir.as_ref();

        match fs::create_dir_all(log_dir) {
            Ok(_) => (),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            Err(e) => anyhow::bail!(format!("invalid log path: {}", e)),
        }

        logger = logger
            .log_to_file(FileSpec::default().directory(log_dir))
            .duplicate_to_stderr(Duplicate::All)
            .rotate(
                Criterion::Size(2 * 1024 * 1024),
                Naming::Timestamps,
                Cleanup::KeepLogFiles(7),
            )
    }

    logger
        .format_for_stderr(log_format)
        .format_for_files(log_format)
        .print_message()
        .start()?;

    Ok(())
}

fn log_format(
    w: &mut dyn std::io::Write,
    now: &mut DeferredNow,
    record: &Record,
) -> Result<(), std::io::Error> {
    use std::time::{Duration, UNIX_EPOCH};
    const DATE_FORMAT_STR: &str = "%Y-%m-%d %H:%M:%S%.3f %z";

    let timestamp = now.now().unix_timestamp_nanos() as u64;
    let date = UNIX_EPOCH + Duration::from_nanos(timestamp);
    let local_date = DateTime::<Local>::from(date);

    write!(
        w,
        "[{} {:5} {}] {}",
        local_date.format(DATE_FORMAT_STR),
        record.level(),
        record.module_path().unwrap_or("<unnamed>"),
        record.args()
    )
}

#[get("/hello/{name}")]
async fn greet(name: web::Path<String>) -> impl Responder {
    format!("Hello {name}!")
}

#[get("/progress")]
async fn progress_endpoint() -> impl Responder {
    {
        let updater_state = UPDATER_STATE.lock().unwrap();
        if let Some(progress) = updater_state
            .lighthouse_updater
            .as_ref()
            .map(|upd| Some(upd.get_progress()))
            .unwrap_or(None)
        {
            return format!("progress: {:?}", progress);
        };
    }

    format!("Update started!")
}

#[get("/start")]
async fn start_update() -> impl Responder {
    {
        let mut updater_state = UPDATER_STATE.lock().unwrap();
        if updater_state
            .lighthouse_updater
            .as_ref()
            .map(|upd| upd.is_running())
            .unwrap_or(false)
        {
            return format!("Already running");
        } else {
            let pd = PipeDownloader::new(
                "http://mumbai-main.golem.network:14372/test.tar.lz4",
                &PathBuf::from("output"),
                PipeDownloaderOptions::default(),
            );
            let mut lighthouse_updater =
                UpdateTask::new(pd, Some("lighthouse-bn.service".to_string()));
            if let Err(e) = lighthouse_updater.run() {
                println!("Error starting update task: {}", e);
                return format!("Error starting update task: {}", e);
            };
            updater_state.lighthouse_updater = Some(lighthouse_updater);
        }
    }

    format!("Update started!")
}

#[get("/pause")]
async fn pause_update() -> impl Responder {
    UPDATER_STATE.lock().unwrap().started = true;
    format!("Update started!")
}

async fn update_loop() -> anyhow::Result<()> {
    loop {
        let is_running = UPDATER_STATE
            .lock()
            .unwrap()
            .lighthouse_updater
            .as_ref()
            .map(|pd| pd.is_running())
            .unwrap_or(false);
        if is_running {
            if let Some(progress) = UPDATER_STATE
                .lock()
                .unwrap()
                .lighthouse_updater
                .as_ref()
                .map(|pd| {
                    pd.downloader
                        .lock()
                        .unwrap()
                        .get_progress()
                        .expect("Failed to get progress")
                        .clone()
                })
            {
                println!(
                    "downloaded: {} speed[current: {}/s total: {}/s], unpacked: {} [current: {}/s total: {}/s]",
                    human_bytes((progress.total_downloaded + progress.chunk_downloaded) as f64),
                    human_bytes(progress.progress_buckets_download.get_speed()),
                    progress.get_download_speed_human(),
                    human_bytes(progress.total_unpacked as f64),
                    human_bytes(progress.progress_buckets_unpack.get_speed()),
                    progress.get_unpack_speed_human(),
                );
            }
        }

        /*{


        }*/

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }
}

#[actix_web::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenv::dotenv();
    let cli: Cli = Cli::from_args();

    //needed for systemctl library
    env::set_var("SYSTEMCTL_PATH", &cli.systemctl_path);

    setup_logging(cli.log_dir.as_ref())?;

    task::spawn(async move {
        match update_loop().await {
            Ok(_) => (),
            Err(e) => log::error!("Error in update loop: {}", e),
        }
    });

    HttpServer::new(|| {
        App::new()
            .route("/", web::get().to(HttpResponse::Ok))
            .service(greet)
            .service(start_update)
            .service(progress_endpoint)
    })
    .bind(("0.0.0.0", 15100))
    .map_err(anyhow::Error::from)?
    .run()
    .await
    .map_err(anyhow::Error::from)
}
