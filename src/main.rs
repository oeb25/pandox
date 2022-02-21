use clap::Parser;
use color_eyre::eyre::WrapErr;
use futures_util::{FutureExt, SinkExt, StreamExt};
use notify::{RecursiveMode, Watcher};
use std::{
    collections::HashSet,
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;
use tracing::info;
use warp::{Filter, Reply};

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
enum Args {
    /// Compile and watch files
    Watch {
        /// Name of the person to greet
        #[clap(short, long)]
        target: Option<PathBuf>,

        /// The port to listen on
        #[clap(short, long, default_value = "1337")]
        port: u16,

        /// Open output
        #[clap(short, long)]
        open: bool,

        /// Files to watch
        files: Vec<PathBuf>,

        /// Forward arguments to pandoc
        #[clap(last = true)]
        pandoc_args: Vec<String>,
    },
    /// Remotely connect to another pandox instance
    Join {
        /// The port to connect on
        #[clap(short, long, default_value = "1337")]
        port: u16,

        host: String,
    },
}

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }
    tracing_subscriber::fmt::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    run().await?;

    Ok(())
}
async fn run() -> color_eyre::Result<()> {
    match Args::parse() {
        Args::Watch {
            target,
            open,
            port,
            files,
            pandoc_args,
        } => {
            let (_dir, target) = if let Some(target) = target {
                (None, target)
            } else {
                let dir = tempdir::TempDir::new("pandox")?;
                let target = dir.path().join("pandox.pdf");
                (Some(dir), target)
            };

            let (sender, receiver) = crossbeam_channel::unbounded();

            let mut watcher = notify::recommended_watcher(move |res: notify::Result<_>| {
                let res = res.unwrap();
                sender.send(res).unwrap();
            })
            .unwrap();
            for file in &files {
                let path = file.canonicalize()?;
                watcher.watch(&path, RecursiveMode::NonRecursive)?;
            }

            watcher.configure(notify::Config::PreciseEvents(true))?;

            let (watch_tx, watch_rx) = tokio::sync::watch::channel("watch output");

            let index =
                warp::path("index.html").map(|| warp::reply::html(include_str!("./preview.html")));
            let pdf = warp::path("output")
                .and(warp::path::param::<String>())
                .and(warp::fs::file(target.clone()))
                .map(|_, file| {
                    warp::reply::with_header(file, "Cache-Control", "no-cache").into_response()
                });
            let ws = warp::path("ws")
                .and(warp::ws())
                .map(move |ws: warp::ws::Ws| {
                    let mut watch_rx = watch_rx.clone();

                    ws.on_upgrade(|ws| {
                        let (mut tx, _rx) = ws.split();

                        async move {
                            while watch_rx.changed().await.is_ok() {
                                let res = tx
                                    .send(warp::ws::Message::text("reload"))
                                    .map(|result| {
                                        if let Err(e) = &result {
                                            tracing::warn!("WebSocket Error: {e:?}");
                                        }
                                        result
                                    })
                                    .await;

                                if res.is_err() {
                                    break;
                                }
                            }
                        }
                    })
                });

            let routes = index.or(pdf).or(ws);

            tokio::spawn(warp::serve(routes).run(([0, 0, 0, 0], port)));

            info!("Output file is {:?}", target);

            let mut first = true;
            let mut force = true;
            let mut changed = HashSet::new();

            loop {
                let res = receiver.recv_timeout(Duration::from_millis(50));
                match res {
                    Ok(e) => {
                        for p in e.paths.clone() {
                            changed.insert(p);
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        if changed.is_empty() && !force {
                            continue;
                        }
                        force = false;

                        changed.clear();

                        let mut pandoc = pandoc::new();
                        for file in &files {
                            pandoc.add_input(file);
                        }
                        pandoc.set_output(pandoc::OutputKind::File(target.clone()));
                        pandoc.forward(&pandoc_args);

                        let start = Instant::now();
                        match pandoc.execute() {
                            Ok(result) => {
                                watch_tx.send("update!").unwrap();
                                if first {
                                    first = false;
                                    if open {
                                        open_pdf(&target)?;
                                    }
                                }
                                info!("File written in {:?}", start.elapsed());
                                match result {
                                    pandoc::PandocOutput::ToFile(_) => {}
                                    pandoc::PandocOutput::ToBuffer(_) => {}
                                    pandoc::PandocOutput::ToBufferRaw(_) => {}
                                }
                            }
                            Err(err) => match err {
                                pandoc::PandocError::BadUtf8Conversion(_) => todo!(),
                                pandoc::PandocError::IoErr(_) => todo!(),
                                pandoc::PandocError::Err(e) => {
                                    let error = String::from_utf8(e.stderr.clone()).unwrap();
                                    tracing::error!("{}", error);
                                }
                                pandoc::PandocError::NoOutputSpecified => todo!(),
                                pandoc::PandocError::NoInputSpecified => todo!(),
                                pandoc::PandocError::PandocNotFound => todo!(),
                            },
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        color_eyre::eyre::bail!("Channel closed")
                    }
                }
            }
        }
        Args::Join { host, port } => {
            let dir = tempdir::TempDir::new("pandox")?;
            let file_path = dir.path().join("pandox.pdf");

            let (mut socket, _) = tokio_tungstenite::tungstenite::connect(
                url::Url::parse(&format!("ws://{host}:{port}/ws")).unwrap(),
            )
            .context("Failed to connect")?;

            info!("Connected to the server");

            info!(?file_path, "Temp file created");

            let mut first = true;

            loop {
                let _msg = socket.read_message().context("Error reading message")?;
                info!("Received update");

                let resp = reqwest::get(&format!("http://{host}:{port}/output/file.pdf"))
                    .await?
                    .bytes()
                    .await?;

                tokio::fs::File::create(&file_path)
                    .await?
                    .write_all(&resp)
                    .await?;

                if first {
                    first = false;
                    open_pdf(&file_path)?;
                }
            }
            // socket.close(None);
        }
    }
}

fn open_pdf(path: impl AsRef<std::path::Path>) -> color_eyre::Result<()> {
    let path = path.as_ref();
    if open::with(path, "skim").is_err() {
        open::that(path)?;
    }
    Ok(())
}
