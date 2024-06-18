#![allow(non_snake_case)]

use chrono::prelude::*;
use percent_encoding::percent_decode_str;
use reqwest::header::{COOKIE, USER_AGENT};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::fs::{self};
use std::io::{self};
use std::io::{BufRead, BufReader};
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, SystemTime};
use structopt::StructOpt;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};
use tracing_subscriber;

#[derive(Debug, StructOpt)]
struct Cli {
    #[structopt(default_value = "", help = "直播ID，不输入则采用交互式方式获取")]
    liveid: String,
    #[structopt(
        short,
        long,
        default_value = "",
        help = "自定义下载命令，使用 {url}, {save_dir}, {filename} 作为替换标记"
    )]
    command: String,
    #[structopt(short, long, help = "仅下载单集视频")]
    single: bool,
}

#[derive(Deserialize, Debug, Clone)]
struct Entry {
    id: u64,
    startTime: StartTime,
    courseCode: String,
    courseName: String,
    days: u64,
    jie: String,
}

#[derive(Deserialize, Debug, Clone)]
struct StartTime {
    time: u64,
    day: u32,
}

#[derive(Deserialize, Debug)]
struct VideoPaths {
    pptVideo: Option<String>,
    teacherTrack: Option<String>,
}

#[derive(Deserialize, Debug)]
struct InfoJson {
    videoPath: VideoPaths,
}

#[derive(Serialize, Deserialize, Clone)]
struct CsvRow {
    live_id: u64,
    month: u32,
    date: u32,
    day: u32,
    jie: String,
    days: u64,
    ppt_video: String,
    teacher_track: String,
}

async fn get_initial_data(live_id: &str) -> Result<Vec<Entry>, Box<dyn std::error::Error>> {
    info!("Fetching initial data for live_id: {}", live_id);
    let client = reqwest::Client::new();
    let url = "http://newesxidian.chaoxing.com/live/listSignleCourse";
    let response = client
        .post(url)
        .header(USER_AGENT, "Mozilla/5.0")
        .header(COOKIE, "UID=2")
        .form(&[("liveId", live_id)])
        .send()
        .await?
        .json::<Vec<Entry>>()
        .await?;
    debug!("Received response: {:?}", response);
    Ok(response)
}

async fn get_m3u8_links(
    live_id: u64,
) -> Result<(Option<String>, Option<String>), Box<dyn std::error::Error + Send + Sync>> {
    info!("Fetching m3u8 links for live_id: {}", live_id);
    let client = reqwest::Client::new();
    let url = format!(
        "http://newesxidian.chaoxing.com/live/getViewUrlHls?liveId={}&status=2",
        live_id
    );

    for attempt in 1..=3 {
        let response_text = client
            .get(&url)
            .header(reqwest::header::USER_AGENT, "Mozilla/5.0")
            .header(reqwest::header::COOKIE, "UID=2")
            .send()
            .await?
            .text()
            .await?;

        debug!("Received m3u8 response text: {}", response_text);

        if let Some(url_start) = response_text.find("info=") {
            let encoded_info = &response_text[url_start + 5..];
            let decoded_info = match percent_decode_str(encoded_info).decode_utf8() {
                Ok(info) => info,
                Err(err) => {
                    error!("Failed to decode info: {}", err);
                    continue;
                }
            };
            let info_json: InfoJson = match serde_json::from_str(&decoded_info) {
                Ok(json) => json,
                Err(err) => {
                    error!("Failed to parse JSON: {}", err);
                    continue;
                }
            };
            debug!("Decoded info JSON: {:?}", info_json);

            let ppt_video = info_json.videoPath.pptVideo.clone();
            let teacher_track = info_json.videoPath.teacherTrack.clone();

            info!(
                "Succeed to get m3u8 link: {:?} and {:?}",
                ppt_video, teacher_track
            );
            return Ok((ppt_video, teacher_track));
        } else {
            error!(
                "info parameter not found in the response: {}",
                response_text
            );
        }

        warn!("Attempt {}/3 failed. Retrying...", attempt);
        sleep(Duration::from_millis(500)).await; // Wait for 500 milliseconds before retrying
    }

    Err("Failed to get m3u8 links after 3 attempts".into())
}
#[cfg(feature = "ffmpeg")]
fn exec_command(url: &str, filename: &str, save_dir: &str, _: &str) -> std::io::Result<Child> {
    use std::path::PathBuf;
    let mut path = PathBuf::new();
    path.push(save_dir);
    path.push(filename);
    Command::new("ffmpeg")
        .arg("-i")
        .arg(url)
        .arg("-f")
        .arg("mp4")
        .arg("-c")
        .arg("copy")
        .arg("")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}
#[cfg(not(feature = "ffmpeg"))]
fn exec_command(
    url: &str,
    filename: &str,
    save_dir: &str,
    command: &str,
) -> std::io::Result<Child> {
    let default_command = if cfg!(target_os = "windows") {
        format!(
            r#"./N_m3u8DL-RE.exe "{}" --save-dir "{}" --save-name "{}" --check-segments-count False --binary-merge True"#,
            url, save_dir, filename
        )
    } else {
        format!(
            r#"N_m3u8DL-RE "{}" --save-dir "{}" --save-name "{}" --check-segments-count False --binary-merge True"#,
            url, save_dir, filename
        )
    };

    let command_to_run = if command.is_empty() {
        default_command
    } else {
        command
            .replace("{url}", url)
            .replace("{save_dir}", save_dir)
            .replace("{filename}", filename)
    };

    debug!("Running download command: {}", command_to_run);

    let result = if cfg!(target_os = "windows") {
        Command::new("powershell")
            .arg("/C")
            .arg(&command_to_run)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    } else if cfg!(feature = "ffmpeg") {
        Command::new("ffmpeg").arg("-i").spawn()
    } else {
        Command::new("sh")
            .arg("-c")
            .arg(&command_to_run)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    };
    result
}

fn download_m3u8(
    url: &str,
    filename: &str,
    save_dir: &str,
    command: &str,
) -> Result<(), Box<dyn Error>> {
    info!(
        "Downloading m3u8: url={} filename={} save_dir={}",
        url, filename, save_dir
    );
    let result = exec_command(url, filename, save_dir, command);

    match result {
        Ok(mut child) => {
            let (tx, rx): (Sender<String>, Receiver<String>) = mpsc::channel();
            let stdout = child.stdout.take().expect("Failed to capture stdout");
            let stderr = child.stderr.take().expect("Failed to capture stderr");

            let tx_clone = tx.clone();
            thread::spawn(move || {
                #[cfg(not(feature = "ffmpeg"))]
                let reader = BufReader::new(stdout);
                #[cfg(feature = "ffmpeg")]
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    match line {
                        Ok(l) => {
                            tx_clone
                                .send(format!("STDOUT: {}", l))
                                .expect("Failed to send stdout");
                        }
                        Err(_e) => {
                            // error!("Error reading stdout: {:?}", e);
                        }
                    }
                }
            });

            thread::spawn(move || {
                #[cfg(not(feature = "ffmpeg"))]
                let reader = BufReader::new(stderr);
                #[cfg(feature = "ffmpeg")]
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    match line {
                        Ok(l) => {
                            tx.send(format!("STDERR: {}", l))
                                .expect("Failed to send stderr");
                        }
                        Err(e) => {
                            error!("Error reading stderr: {:?}", e);
                        }
                    }
                }
            });

            for msg in rx {
                if msg.starts_with("STDOUT") {
                    info!(
                        "下载 {} 标准输出: {}",
                        filename,
                        msg.trim_start_matches("STDOUT: ")
                    );
                } else if msg.starts_with("STDERR") {
                    error!(
                        "下载 {} 标准错误: {}",
                        filename,
                        msg.trim_start_matches("STDERR: ")
                    );
                }
            }

            let output = child.wait().expect("Child process wasn't running");
            if !output.success() {
                error!(
                    "下载 {} 失败，exit code: {}",
                    filename,
                    output.code().unwrap_or(-1)
                );
            }
        }
        Err(err) => {
            error!("下载 {} 出错: {:?}，跳过此视频。", filename, err);
        }
    }

    Ok(())
}

fn day_to_chinese(day: u32) -> &'static str {
    match day {
        0 => "日",
        1 => "一",
        2 => "二",
        3 => "三",
        4 => "四",
        5 => "五",
        6 => "六",
        _ => "",
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing subscriber for logging
    tracing_subscriber::fmt::init();

    let args = Cli::from_args();
    let liveid_from_cli = if args.liveid.is_empty() {
        println!("请输入 liveId：");
        let mut input_live_id = String::new();
        io::stdin().read_line(&mut input_live_id)?;
        input_live_id.trim().to_string()
    } else {
        args.liveid
    };

    info!("Starting with liveid: {}", liveid_from_cli);

    let input_live_id = liveid_from_cli.trim();
    let data = get_initial_data(input_live_id).await?;

    if data.is_empty() {
        warn!("没有找到数据，请检查 liveId 是否正确。");
        return Ok(());
    }

    let first_entry = data.first().unwrap().clone();
    let start_time = first_entry.startTime.time;
    let course_code = &first_entry.courseCode;
    let course_name = &first_entry.courseName;

    let start_time_unix = start_time / 1000;
    let start_time_struct =
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(start_time_unix);
    let datetime: chrono::DateTime<chrono::Utc> = start_time_struct.into();
    let year = datetime.year();
    let save_dir = format!("./output/{}年{}{}", year, course_code, course_name);
    fs::create_dir_all(&save_dir)?;

    let csv_filename = format!("./output/{}年{}{}.csv", year, course_code, course_name);

    let mut cache: HashMap<u64, CsvRow> = HashMap::new();
    if let Ok(reader) = csv::Reader::from_path(&csv_filename) {
        for result in reader.into_deserialize() {
            if let Ok(row) = result {
                let row: CsvRow = row;
                cache.insert(row.live_id, row);
            }
        }
    }

    let mut writer = csv::WriterBuilder::new().has_headers(false).from_writer(
        match fs::OpenOptions::new().append(true).open(&csv_filename) {
            Ok(file) => file,
            Err(_) => {
                // If the file does not exist, create it
                fs::File::create(&csv_filename)?
            }
        },
    );

    let mut rows: Vec<CsvRow> = vec![];
    for entry in data {
        let live_id = entry.id;
        if args.single && live_id.to_string() != input_live_id {
            continue;
        }

        // Check cache first
        if let Some(cached_row) = cache.get(&live_id).cloned() {
            rows.push(cached_row);
            continue;
        }

        let result = get_m3u8_links(live_id).await;
        let (ppt_video, teacher_track) = result.unwrap_or_default();

        let start_time = entry.startTime.time;
        let start_time_unix = start_time / 1000;
        let start_time_struct =
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(start_time_unix);
        let datetime: chrono::DateTime<chrono::Utc> = start_time_struct.into();
        let month = datetime.month();
        let date = datetime.day();

        let ppt_video_str = ppt_video.unwrap_or_default();
        let teacher_track_str = teacher_track.unwrap_or_default();

        let row = CsvRow {
            live_id,
            month,
            date,
            day: entry.startTime.day,
            jie: entry.jie.clone(),
            days: entry.days.clone(),
            ppt_video: ppt_video_str.clone(),
            teacher_track: teacher_track_str.clone(),
        };

        writer.serialize(&row)?;
        rows.push(row);
    }

    for row in rows {
        if !row.ppt_video.is_empty() {
            let ppt_video_filename = format!(
                "{}月{}日_星期{}_{}节_ppt.m3u8",
                row.month,
                row.date,
                day_to_chinese(row.day),
                row.jie
            );
            download_m3u8(
                &row.ppt_video,
                &ppt_video_filename,
                &save_dir,
                &args.command,
            )?;
        }

        if !row.teacher_track.is_empty() {
            let teacher_track_filename = format!(
                "{}月{}日_星期{}_{}节_主讲.m3u8",
                row.month,
                row.date,
                day_to_chinese(row.day),
                row.jie
            );
            download_m3u8(
                &row.teacher_track,
                &teacher_track_filename,
                &save_dir,
                &args.command,
            )?;
        }
    }

    info!("Process completed successfully.");
    Ok(())
}
