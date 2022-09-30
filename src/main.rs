use std::path::PathBuf;
use std::{env, fs, thread};

use actix_web::{get, web, App, HttpResponse, HttpServer, Responder};

use lazy_static::lazy_static; // 1.4.0
use pipe_downloader::pipe_downloader::{PipeDownloader, PipeDownloaderOptions};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use structopt::StructOpt;
use tokio::task;

struct UpdateTask {
    downloader: Arc<Mutex<PipeDownloader>>,
    process_to_close: Option<String>,
    is_running: Arc<Mutex<bool>>,
    paths_to_remove: Vec<PathBuf>,
    target_user: Option<String>,
    target_group: Option<String>,
    target_path: Option<PathBuf>,
}

fn update_task_main(
    process_to_close: Option<String>,
    downloader: Arc<Mutex<PipeDownloader>>,
    paths_to_remove: Vec<PathBuf>,
    target_user: Option<String>,
    target_group: Option<String>,
    target_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    if let Some(process_to_close) = process_to_close.as_ref() {
        log::info!("Stopping process: {}", process_to_close);
        match systemctl::stop(process_to_close) {
            Ok(_) => {
                log::info!("Process stopped: {}", process_to_close);
            }
            Err(e) => {
                log::error!("Failed to stop process {}: {}", process_to_close, e);
                return Err(anyhow::anyhow!(
                    "Failed to stop process {}: {}",
                    process_to_close,
                    e
                ));
            }
        };
    }
    for path in paths_to_remove {
        if path.is_dir() {
            log::info!("Removing directory: {}", path.display());
            if let Err(err) = fs::remove_dir_all(&path) {
                log::error!("Error removing directory: {}", err);
                return Err(anyhow::anyhow!("Error removing directory: {}", err));
            }
        } else if path.is_file() {
            log::info!("Removing file: {}", path.display());
            if let Err(err) = fs::remove_file(&path) {
                log::error!("Error removing file: {}", err);
                return Err(anyhow::anyhow!("Error removing file: {}", err));
            }
        } else {
            log::info!("Trying to remove, path not exists: {}", path.display());
        }
    }

    //let system see that the directories are removed
    thread::sleep(Duration::from_secs(1));

    log::info!("Start download");

    match downloader.lock().unwrap().start_download() {
        Ok(_) => {}
        Err(e) => {
            log::error!("Error started downloading: {}", e);
            return Err(anyhow::anyhow!("Error started downloading: {}", e));
        }
    };

    while !downloader.lock().unwrap().is_finished() {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    log::info!("Download finished");

    if let (Some(target_user), Some(target_group), Some(target_path)) =
        (target_user, target_group, target_path)
    {
        log::info!(
            "Changing path ownership: {} to {}:{}",
            target_path.display(),
            target_user,
            target_group
        );
        let command = std::format!(
            "chown -R {}:{} {}",
            target_user,
            target_group,
            target_path.display()
        )
        .to_string();
        match std::process::Command::new("/bin/bash")
            .arg("-c")
            .arg(command)
            .output()
        {
            Ok(_) => {}
            Err(e) => {
                println!("Error changing owner: {}", e);
                return Err(anyhow::anyhow!("Error changing owner: {}", e));
            }
        };
    }

    if let Some(error_message) = downloader.lock().unwrap().get_progress().error_message {
        log::error!("Error downloading: {}", error_message);
        return Err(anyhow::anyhow!(
            "Download failed with error: {}",
            error_message
        ));
    }

    if let Some(process_to_close) = process_to_close.as_ref() {
        log::info!("Starting process: {}", process_to_close);
        match systemctl::restart(process_to_close) {
            Ok(_) => {}
            Err(e) => {
                log::error!("Error restarting service: {}", e);
                return Err(anyhow::anyhow!("Error starting service: {}", e));
            }
        };
    }
    Ok(())
}

impl UpdateTask {
    fn new(
        downloader: PipeDownloader,
        process_to_close: Option<String>,
        paths_to_remove: Vec<PathBuf>,
        target_user: Option<String>,
        target_group: Option<String>,
        target_path: Option<PathBuf>,
    ) -> Self {
        Self {
            downloader: Arc::new(Mutex::new(downloader)),
            process_to_close,
            is_running: Arc::new(Mutex::new(false)),
            paths_to_remove,
            target_user,
            target_group,
            target_path,
        }
    }

    fn is_running(self: &Self) -> bool {
        *self.is_running.lock().unwrap()
    }

    fn get_progress(self: &Self) -> serde_json::Value {
        self.downloader.lock().unwrap().get_progress_json()
    }

    fn run(self: &mut Self) -> anyhow::Result<()> {
        if *self.is_running.lock().unwrap() {
            return Err(anyhow::anyhow!("Task is already running"));
        }
        *self.is_running.lock().unwrap() = true;
        let process_to_close = self.process_to_close.clone();
        let downloader = self.downloader.clone();
        let is_running = self.is_running.clone();
        let paths_to_remove = self.paths_to_remove.clone();
        let target_user = self.target_user.clone();
        let target_group = self.target_group.clone();
        let target_path = self.target_path.clone();

        thread::spawn(move || {
            match update_task_main(
                process_to_close,
                downloader,
                paths_to_remove,
                target_user,
                target_group,
                target_path,
            ) {
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
    updater: Option<UpdateTask>,
}

lazy_static! {
    static ref UPDATER_STATE: Arc<Mutex<AppState>> = Arc::new(Mutex::new(AppState {
        started: false,
        updater: None
    }));
}

#[derive(StructOpt, Debug)]
struct Cli {
    /// Listen address
    #[structopt(long, default_value = "/usr/bin/systemctl")]
    pub systemctl_path: PathBuf,

    /// Listen address
    #[structopt(long, default_value = "127.0.0.1")]
    pub listen_addr: String,

    /// Listen port
    #[structopt(long, default_value = "15100")]
    pub listen_port: u16,
}

#[get("/hello/{name}")]
async fn greet(name: web::Path<String>) -> impl Responder {
    format!("Hello {name}!")
}

#[get("/progress")]
async fn progress_endpoint() -> impl Responder {
    let updater_state = UPDATER_STATE.lock().unwrap();
    if let Some(progress) = updater_state
        .updater
        .as_ref()
        .map(|upd| Some(upd.get_progress()))
        .unwrap_or(None)
    {
        return web::Json(progress);
    };
    return web::Json(serde_json::json!({}));
}

#[get("/start")]
async fn start_update() -> impl Responder {
    {
        let mut updater_state = UPDATER_STATE.lock().unwrap();
        if updater_state
            .updater
            .as_ref()
            .map(|upd| upd.is_running())
            .unwrap_or(false)
        {
            return format!("Already running");
        } else {
            let output_dir =
                PathBuf::from(env::var("OUTPUT_DIR").unwrap_or_else(|_| "output".into()));
            let delete_dirs = env::var("DELETE_DIRS")
                .unwrap_or_else(|_| "output".into())
                .split(";")
                .map(|s| PathBuf::from(s))
                .collect::<Vec<_>>();

            let process_to_close = env::var("PROCESS_CLOSE").map(|v| Some(v)).unwrap_or(None);
            let target_user = env::var("TARGET_USER").unwrap_or_else(|_| "erigon".into());
            let target_group = env::var("TARGET_GROUP").unwrap_or_else(|_| "erigon".into());
            let target_change_owner_dir =
                PathBuf::from(env::var("CHANGE_OWNER_PATH").unwrap_or_else(|_| "erigon".into()));

            let url = env::var("ARCHIVE_URL")
                .unwrap_or_else(|_| "http://mumbai-main.golem.network:14372/beacon.tar.lz4".into());

            let pd = PipeDownloader::new(&url, &output_dir, PipeDownloaderOptions::default());

            let mut updater = UpdateTask::new(
                pd,
                process_to_close,
                delete_dirs,
                Some(target_user),
                Some(target_group),
                Some(target_change_owner_dir),
            );
            if let Err(e) = updater.run() {
                println!("Error starting update task: {}", e);
                return format!("Error starting update task: {}", e);
            };
            updater_state.updater = Some(updater);
        }
    }

    format!("Update started!")
}

#[get("/pause")]
async fn pause_update() -> impl Responder {
    UPDATER_STATE.lock().unwrap().started = true;
    format!("Update started!")
}

// for debug only, it can be disabled in production
async fn update_loop() -> anyhow::Result<()> {
    loop {
        let is_running = UPDATER_STATE
            .lock()
            .unwrap()
            .updater
            .as_ref()
            .map(|pd| pd.is_running())
            .unwrap_or(false);
        if is_running {
            if let Some(progress_human_line) = UPDATER_STATE
                .lock()
                .unwrap()
                .updater
                .as_ref()
                .map(|pd| pd.downloader.lock().unwrap().get_progress_human_line())
            {
                log::debug!("{}", progress_human_line);
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
    env_logger::init();

    //needed for systemctl library
    env::set_var("SYSTEMCTL_PATH", &cli.systemctl_path);

    task::spawn(async move {
        match update_loop().await {
            Ok(_) => (),
            Err(e) => log::error!("Error in update loop: {}", e),
        }
    });

    log::info!(
        "Starting update server: {}:{}",
        cli.listen_addr,
        cli.listen_port
    );

    HttpServer::new(|| {
        App::new()
            .route("/", web::get().to(HttpResponse::Ok))
            .service(greet)
            .service(start_update)
            .service(progress_endpoint)
    })
    .workers(1)
    .bind((cli.listen_addr, cli.listen_port))
    .map_err(anyhow::Error::from)?
    .run()
    .await
    .map_err(anyhow::Error::from)
}
