use crate::config::FileNameConflictResolutionStrategy;
use async_logger;
use axum::{
    Router,
    body::Body,
    extract::{ConnectInfo, DefaultBodyLimit, FromRequest, Multipart, State},
    http::{HeaderMap, Method, Request, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use axum_extra::response::file_stream::FileStream;
use chrono::{DateTime, Utc};
use clap::Parser;
use log::*;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tera::{Context, Tera};
use tokio::{
    fs::File,
    io::AsyncWriteExt,
    signal,
    sync::{Semaphore, broadcast},
};
use tokio_util::{io::ReaderStream, sync::CancellationToken};

mod config;
mod utils;

const APP_NAME: &str = "LwNAS";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const AUTHOR: &str = "Que's Software";
const SERVER: &str = constcat::concat!(APP_NAME, "/v", VERSION);

type ThumbsWaitingMap = HashMap<String, broadcast::Sender<bool>>;

struct AppState {
    conf: config::Config,
    templates: Tera,
    // 关闭服务器通道
    token: CancellationToken,
    // 写上传文件的全局锁,先异步写入 uuid 临时文件,完成后在锁内完成原子性重名校验和重命名
    fs_lock: Mutex<()>,
    // 缩略图文件全局锁,判断缩略图是否存在,写入都必须在锁内完成
    thumbs_lock: Mutex<ThumbsWaitingMap>,
    thumbs_max_parallel_sem: Arc<Semaphore>,
}

#[derive(Parser, Debug)]
#[command(
    version,
    long_about,
    about = constcat::concat!(APP_NAME, " home photoes sharing web server"),
    name = APP_NAME,
)]
struct Args {
    /// configure file
    config: std::path::PathBuf,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let conf = config::load_from_file(&args.config).expect("failed to load configure file");

    // 初始化日志模块
    let async_logger_holder = async_logger::Builder::new()
        .bound(conf.logger.bound)
        .max_level(conf.logger.level)
        .stdout(conf.logger.stdout)
        .stderr(conf.logger.stderr)
        .rotated_file(
            &conf.logger.rf_file_name,
            conf.logger.rf_file_size,
            conf.logger.rf_file_count,
        )
        .setup();

    info!("Welcome to {} v{} by {}", APP_NAME, VERSION, AUTHOR);

    // 创建共享状态
    let mut templates = Tera::default();
    templates.load_from_glob(&conf.templates).expect(&format!(
        "failed to load Tera templates from \"{}\"",
        conf.templates
    ));

    let token = CancellationToken::new();
    let app_state = Arc::new(AppState {
        templates: templates,
        token: token.clone(),
        fs_lock: Mutex::new(()),
        thumbs_lock: Mutex::new(ThumbsWaitingMap::new()),
        thumbs_max_parallel_sem: Arc::new(Semaphore::new(conf.thumb_max_parallel as usize)),
        conf: conf,
    });

    // 启动 httpd
    let listener = tokio::net::TcpListener::bind(&app_state.conf.addr)
        .await
        .expect(&format!("tcp address {} unavailable", app_state.conf.addr));
    info!("httpd runing on: {} ...", app_state.conf.addr);

    // 创建路由表,添加固定路由
    let app = Router::new()
        .route("/", get(root))
        .route("/shutdown", get(shutdown))
        .fallback(fallback)
        .layer(DefaultBodyLimit::max(
            app_state.conf.max_upload_limit as usize,
        ))
        .layer(middleware::from_fn(lwnas_layer))
        .with_state(app_state)
        .into_make_service_with_connect_info::<SocketAddr>();

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = token.cancelled() => {},
                r = signal::ctrl_c() => { r.expect("tokio::signal_ctrl_c failed") },
            }
        })
        .await
        .expect("failed to start axum server");

    info!("httpd stopped");
    info!("Bye");

    async_logger_holder.shutdown().await;
}

// 参考千问AI: 当中间件被 axum 框架调用时, request 处于请求头已经接收完毕,但 body 尚未开始读取的状态
// 所以才可以通过包装 Body 读取器实现限流,即 tower_http::limit::RequestBodyLimitLayer 中间件的基本原理
async fn lwnas_layer(request: Request<Body>, next: Next) -> Result<Response, StatusCode> {
    let method = request.method().clone();
    let uri = request.uri().clone();

    // debug!("request headers: {:?}", request.headers());
    // 获取客户端 IP 地址
    let client_ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip().to_string())
        .unwrap_or("<no ip>".to_string());

    // 计时
    let start = std::time::Instant::now();
    let mut response = next.run(request).await;
    let elapsed = start.elapsed();

    // 统一添加 Server 响应头
    response
        .headers_mut()
        .insert(header::SERVER, header::HeaderValue::from_static(SERVER));

    info!(
        "[{}] {} {} -> {} [{} ms]",
        client_ip,
        method,
        uri,
        response.status(),
        elapsed.as_millis()
    );
    // debug!("response headers: {:?}", response.headers());

    Ok(response)
}

async fn root(State(app_state): State<Arc<AppState>>) -> Response {
    let mut context = Context::new();
    context.insert("APP_NAME", APP_NAME);
    context.insert("VERSION", VERSION);
    context.insert("AUTHOR", AUTHOR);
    context.insert("TITLE", "/");

    let mut entries: Vec<utils::RootFileEntryDesc> = Vec::new();
    for i in &app_state.conf.root_paths {
        if !i.hide.is_some_and(|x| x) {
            let writable = i.writable.is_some_and(|x| x);
            let deletable = i.deletable.is_some_and(|x| x);
            let mut size = String::new();
            let mut last_modified = String::new();

            if let Ok(attr) = tokio::fs::metadata(&i.local_path).await {
                if let Ok(modified) = attr.modified() {
                    let dt: DateTime<Utc> = modified.into();
                    last_modified = dt.format("%Y-%m-%d %H:%M:%S UTC").to_string();
                }

                if attr.is_dir() {
                    size = "<DIR>".to_string();
                } else {
                    size = utils::fmt_human_size(attr.len());
                }
            }

            entries.push(utils::RootFileEntryDesc {
                entry: utils::FileEntryDesc {
                    name: i.uri_path.clone(),
                    size: size,
                    last_modified: last_modified,
                    url: utils::encode_uri(&i.uri_path),
                },
                permission: utils::fmt_permission(writable, deletable),
            });
        }
    }
    context.insert("entries", &entries);
    context.insert("shutdown_enabled", &app_state.conf.shutdown_enabled);

    if let Ok(s) = app_state.templates.render("root.html", &context) {
        (StatusCode::OK, Html(s)).into_response()
    } else {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(format!("template \"root.html\" render error")),
        )
            .into_response()
    }
}

async fn shutdown(State(app_state): State<Arc<AppState>>) -> Response {
    if !app_state.conf.shutdown_enabled {
        return StatusCode::FORBIDDEN.into_response();
    }

    app_state.token.cancel();

    let mut context = Context::new();
    context.insert("APP_NAME", APP_NAME);
    context.insert("VERSION", VERSION);
    context.insert("AUTHOR", AUTHOR);

    let header_conn_close = [("Connection", "close")];
    if let Ok(s) = app_state.templates.render("bye.html", &context) {
        (StatusCode::OK, header_conn_close, Html(s)).into_response()
    } else {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            header_conn_close,
            Html(format!("template \"bye.html\" render error")),
        )
            .into_response()
    }
}

async fn fallback(State(app_state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    // 通用只读元数据
    let uri = request.uri();
    let method = request.method();
    let headers = request.headers();

    // url 解码
    let Ok(path) = utils::decode_uri(uri.path()) else {
        return (StatusCode::BAD_REQUEST, format!("url decode error")).into_response();
    };

    // 规范化 uri,移除 ../ ./
    let Ok(path) = utils::normalize_uri_path(&path) else {
        return (StatusCode::BAD_REQUEST, format!("invalid path")).into_response();
    };

    // 配置中指定的目录, 静态文件
    for i in &app_state.conf.root_paths {
        // TODO: 考虑大小写的问题
        if path.starts_with(&i.uri_path) {
            // 前缀部分替换为 local_path 构成本地路径
            let writable = i.writable.is_some_and(|x| x);
            let deletable = i.deletable.is_some_and(|x| x);
            let mut local_path = PathBuf::from(&i.local_path);

            let rest = path
                .strip_prefix(&i.uri_path)
                .expect("failed to strip prefix");

            if !rest.is_empty() {
                local_path.push(rest);
                // 不替换路径分隔符在 Windows 也能正常工作,根据 AI 建议,以下无必要
                // local_path.push(rest.replace('/', std::path::MAIN_SEPARATOR_STR));
            }

            debug!("transfer to local \"{}\" ...", local_path.display());

            // 静态文件或者列目录
            if local_path.is_file() {
                if method == Method::DELETE {
                    if writable && deletable {
                        return fallback_to_file_delete(app_state, &local_path, &path).await;
                    } else {
                        return StatusCode::FORBIDDEN.into_response();
                    }
                } else {
                    // query 命令入口
                    if uri.query() == Some("thumb=true") {
                        // 判断 local_path 是不是 image
                        let content_type = utils::guess_mime_type(&local_path);
                        if !utils::is_image(&content_type) {
                            return (
                                StatusCode::BAD_REQUEST,
                                format!("invalid thumbnail file type"),
                            )
                                .into_response();
                        }

                        if app_state.conf.thumb_enabled {
                            return fallback_to_image_thumb(app_state, &local_path, &path).await;
                        } else {
                            return StatusCode::FORBIDDEN.into_response();
                        }
                    } else {
                        return fallback_to_file_get(app_state, &local_path, headers).await;
                    }
                }
            } else if local_path.is_dir() {
                if method == Method::POST {
                    if writable {
                        // 提取 Multipart
                        let multipart = match Multipart::from_request(request, &app_state).await {
                            Ok(multipart) => multipart,
                            _ => return (StatusCode::BAD_REQUEST, "bad multipart").into_response(),
                        };
                        return fallback_to_dir_upload(app_state, &local_path, &path, multipart)
                            .await;
                    } else {
                        return StatusCode::FORBIDDEN.into_response();
                    }
                } else if method == Method::DELETE {
                    if writable && deletable {
                        return fallback_to_dir_delete(app_state, &local_path, &path).await;
                    } else {
                        return StatusCode::FORBIDDEN.into_response();
                    }
                } else {
                    return fallback_to_dir_get(app_state, &local_path, &path, writable, deletable)
                        .await;
                }
            } else {
                // 其他类型都返回 404
            }
            break;
        }
    }

    StatusCode::NOT_FOUND.into_response()
}

async fn fallback_to_file_get<P: AsRef<Path> + Copy>(
    _app_state: Arc<AppState>,
    local_path: P,
    headers: &HeaderMap,
) -> Response {
    // 获取元信息,应该能成功,因为 fallback() 中已经调用过 Path::is_file()
    let attr = match tokio::fs::metadata(&local_path).await {
        Ok(v) => v,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let etag = utils::make_etag_from_metadata(&attr);
    let modified: Option<DateTime<Utc>> = attr.modified().ok().map(DateTime::from);

    // 检查缓存是否匹配
    if let Some(if_none_match) = headers.get(header::IF_NONE_MATCH) {
        if let Ok(if_none_match) = if_none_match.to_str() {
            if if_none_match == etag {
                return StatusCode::NOT_MODIFIED.into_response();
            }
        }
    }

    if let Some(modified) = modified.as_ref() {
        if let Some(if_modified_since) = headers.get(header::IF_MODIFIED_SINCE) {
            if let Ok(if_modified_since) = if_modified_since.to_str() {
                if let Ok(if_modified_since) = DateTime::parse_from_rfc2822(if_modified_since) {
                    if if_modified_since >= *modified {
                        return StatusCode::NOT_MODIFIED.into_response();
                    }
                }
            }
        }
    }

    // 处理请求头中 Range 字段,实现断点续传
    let res = {
        if let Some(range) = headers.get(header::RANGE) {
            if let Ok(range) = range.to_str() {
                if let Ok((range_start, range_end)) = utils::parse_range(range, attr.len()) {
                    // 构建 206 响应
                    match FileStream::<ReaderStream<File>>::try_range_response(
                        local_path,
                        range_start,
                        range_end,
                    )
                    .await
                    {
                        Ok(response) => response,
                        _ => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                    }
                } else {
                    return StatusCode::BAD_REQUEST.into_response();
                }
            } else {
                return StatusCode::BAD_REQUEST.into_response();
            }
        } else {
            // 构建 200 响应
            match FileStream::<ReaderStream<File>>::from_path(&local_path).await {
                Ok(file_stream) => file_stream.into_response(),
                _ => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
    };

    // 构建额外的响应头
    let mut res_headers = HeaderMap::new();

    // 对于静态文件,设置 E-tag, Modify 尽量利用浏览器的缓存
    res_headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("max-age=86400"),
    );

    // 静态文件支持 206
    res_headers.insert(
        header::ACCEPT_RANGES,
        header::HeaderValue::from_static("bytes"),
    );

    // to_rfc2822 should be fine
    if let Some(modified) = modified.as_ref() {
        res_headers.insert(
            header::LAST_MODIFIED,
            modified.to_rfc2822().parse().unwrap(),
        );
    }

    // header parse should be fine
    res_headers.insert(header::ETAG, etag.parse().unwrap());

    // 推断 Content-Type
    let content_type = utils::guess_mime_type(&local_path);
    res_headers.insert(header::CONTENT_TYPE, content_type.parse().unwrap());

    // 对于图片,文本,视频嵌入页面显示/播放
    if utils::is_text(&content_type)
        || utils::is_image(&content_type)
        || utils::is_audio(&content_type)
        || utils::is_video(&content_type)
        || utils::is_pdf(&content_type)
    {
        res_headers.insert(
            header::CONTENT_DISPOSITION,
            header::HeaderValue::from_static("inline"),
        );
    }

    (res_headers, res).into_response()
}

async fn fallback_to_dir_get<P: AsRef<Path> + Copy>(
    app_state: Arc<AppState>,
    local_path: P,
    path: &str,
    writable: bool,
    deletable: bool,
) -> Response {
    let mut context = Context::new();
    context.insert("APP_NAME", APP_NAME);
    context.insert("VERSION", VERSION);
    context.insert("AUTHOR", AUTHOR);
    context.insert("TITLE", path);

    // dirs, image, text, video, ohters 分类列表
    let mut texts: Vec<utils::FileEntryDesc> = Vec::new();
    let mut images = texts.clone();
    let mut videos = texts.clone();
    let mut audios = texts.clone();
    let mut others = texts.clone();
    let mut dirs = texts.clone();

    let mut entries = match tokio::fs::read_dir(&local_path).await {
        Ok(v) => v,
        Err(e) => {
            warn!(
                "faild to read dir \"{}\", {}",
                local_path.as_ref().display(),
                e
            );
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    loop {
        let entry = match entries.next_entry().await {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "failed to read next entry of dir \"{}\", {}",
                    local_path.as_ref().display(),
                    e
                );
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }; 

        let Some(entry) = entry else {
            break;
        };

        let metadata = match entry.metadata().await {
            Ok(v) => v,
            Err(e) => {
                warn!("failed to read metadata, {}", e);
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };

        // 文件/子目录名需要出现在页面和 URL 中,不能接受 to_string_lossy
        let Ok(mut name) = entry.file_name().into_string() else {
            warn!(
                "failed to convert \"{}\" to utf-8 string, ignored",
                entry.file_name().display()
            );
            continue;
        };
        let mut size = String::new();
        let mut last_modified = String::new();
        let mut url = utils::encode_uri(path) + &utils::encode_uri(&name);

        let ls = if metadata.is_dir() {
            name.push_str("/");
            size.push_str("<DIR>");
            url.push_str("/");
            &mut dirs
        } else {
            size = utils::fmt_human_size(metadata.len());

            let content_type = utils::guess_mime_type(&name);
            if utils::is_text(&content_type) || utils::is_pdf(&content_type) {
                &mut texts
            } else if utils::is_image(&content_type) {
                &mut images
            } else if utils::is_audio(&content_type) {
                &mut audios
            } else if utils::is_video(&content_type) {
                &mut videos
            } else {
                &mut others
            }
        };

        if let Ok(modified) = metadata.modified() {
            let dt: DateTime<Utc> = modified.into();
            last_modified = dt.format("%Y-%m-%d %H:%M:%S UTC").to_string();
        }

        ls.push(utils::FileEntryDesc {
            name: name,
            size: size,
            last_modified: last_modified,
            url: url,
        });
    }

    // 按文件名排序
    let f = |x: &utils::FileEntryDesc, y: &utils::FileEntryDesc| x.name.cmp(&y.name);
    texts.sort_by(f);
    images.sort_by(f);
    videos.sort_by(f);
    audios.sort_by(f);
    others.sort_by(f);
    dirs.sort_by(f);

    // ls.html 模板中确认删除的 js 代码依赖字符串 true/false 值
    context.insert(
        "confirm_delete",
        if app_state.conf.confirm_delete {
            "true"
        } else {
            "false"
        },
    );

    context.insert("thumb_enabled", &app_state.conf.thumb_enabled);
    context.insert("writable", &writable);
    context.insert("deletable", &deletable);
    context.insert("dirs", &dirs);
    context.insert("texts", &texts);
    context.insert("images", &images);
    context.insert("audios", &audios);
    context.insert("videos", &videos);
    context.insert("others", &others);
    context.insert("up", utils::get_up_uri_path(path));

    if let Ok(s) = app_state.templates.render("ls.html", &context) {
        (StatusCode::OK, Html(s)).into_response()
    } else {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(format!("template \"ls.html\" render error")),
        )
            .into_response()
    }
}

async fn fallback_to_dir_upload<P: AsRef<Path> + Copy>(
    app_state: Arc<AppState>,
    local_path: P,
    path: &str,
    mut multipart: Multipart,
) -> Response {
    loop {
        let Ok(field) = multipart.next_field().await else {
            debug!("bad multipart data");
            return StatusCode::BAD_REQUEST.into_response();
        };

        let Some(mut field) = field else {
            // 数据序列完结
            break;
        };

        // 原始文件名为空时忽略
        let file_name = field.file_name().unwrap_or("").to_string();
        if file_name.is_empty() {
            continue;
        }

        // 接收输入流写入临时文件(uuid 临时文件,理论上不会冲突)
        let tmp_path = utils::make_unique_tmp_file_name(&app_state.conf.tmp_file_dir);
        let Ok(mut tmp_file) = File::create(&tmp_path).await else {
            warn!("create tmp file \"{}\" failed", tmp_path.display());
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };

        // TODO: 因为重名冲突处理策略可能会导致上传文件被丢弃,此无效上传能否避免?
        // 需要提前锁定文件名才能解决

        // 接收上传
        let mut total_bytes = 0u64;
        loop {
            let chunk = match field.chunk().await {
                Ok(chunk) => chunk,
                Err(e) => {
                    // TODO: 中途退出应该删除临时文件
                    warn!(
                        "failed to reve chunk data, stop at {} bytes: {}",
                        total_bytes, e
                    );
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            };

            let Some(chunk) = chunk else {
                break;
            };

            total_bytes += chunk.len() as u64;
            let Ok(_) = tmp_file.write_all(&chunk).await else {
                warn!("write tmp file \"{}\" failed", tmp_path.display());
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            };
        }

        let Ok(_) = tmp_file.flush().await else {
            warn!("flush tmp file \"{}\" failed", tmp_path.display());
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        drop(tmp_file);

        // *锁内执行* (只能使用 std::fs 接口)
        {
            let mut target_path = local_path.as_ref().to_path_buf();

            // lock unwrap should be fine
            let _lock = app_state.fs_lock.lock().unwrap();

            // 如果原始文件名中含有目录(上传文件夹),则创建目录树
            if let Some(pos) = file_name.rfind('/') {
                target_path.push(&file_name[0..pos]);
                let _ = std::fs::create_dir_all(&target_path);

                target_path.push(&file_name[(pos + 1)..]);
            } else {
                target_path.push(&file_name);
            }

            // 由重名策略校验,生成目标文件名,对于 AppendTimestamp (YYYYMMDD-HHMMSS-MS)可能需要多次尝试
            let resolved_target_path = loop {
                if let Ok(attr) = std::fs::metadata(&target_path) {
                    if attr.is_file() {
                        // 应用重名冲突处理策略
                        match app_state.conf.file_name_conflict_resolution_strategy {
                            FileNameConflictResolutionStrategy::Overwrite => {
                                break Some(target_path);
                            }
                            FileNameConflictResolutionStrategy::Skip => {
                                break None;
                            }
                            FileNameConflictResolutionStrategy::AppendUuid => {
                                target_path = utils::make_unique_file_name_with_uuid(&target_path);
                            }
                            FileNameConflictResolutionStrategy::AppendTimestamp => {
                                target_path =
                                    utils::make_unique_file_name_with_timestamp(&target_path);
                            }
                        }
                    } else {
                        // 其他情况: 目录,硬件符号链接等不替换
                        warn!(
                            "unable to write \"{}\", it's a dir or symbol",
                            target_path.display()
                        );
                        break None;
                    }
                } else {
                    break Some(target_path);
                }
            };

            // 重命名临时文件为目标文件,失败或者跳过时移除临时文件
            if let Some(target_path) = resolved_target_path {
                if let Some(e) = std::fs::rename(&tmp_path, &target_path).err() {
                    warn!(
                        "rename \"{}\" to \"{}\" failed: {}",
                        tmp_path.display(),
                        target_path.display(),
                        e
                    );
                } else {
                    // 只有重命名成功时继续,其他情况需要执行删除临时文件
                    info!(
                        "\"{}\" {} saved as {}",
                        file_name,
                        utils::fmt_human_size(total_bytes),
                        target_path.display()
                    );
                    continue;
                }
            } else {
                info!("skip \"{}\"", file_name);
            }

            // 删除临时文件
            if let Some(e) = std::fs::remove_file(&tmp_path).err() {
                warn!("rename tmp file \"{}\" failed: {}", tmp_path.display(), e);
            } else {
                debug!("tmp file \"{}\" removed", tmp_path.display(),);
            }
        }
    }

    Redirect::to(&utils::encode_uri(path)).into_response()
}

async fn fallback_to_file_delete<P: AsRef<Path> + Copy>(
    app_state: Arc<AppState>,
    local_path: P,
    path: &str,
) -> Response {
    {
        let _lock = app_state.fs_lock.lock().unwrap();

        // 删除文件
        match std::fs::remove_file(&local_path) {
            Ok(_) => info!("file \"{}\" removed", local_path.as_ref().display()),
            Err(e) => warn!(
                "file remove \"{}\" failed: {}",
                local_path.as_ref().display(),
                e
            ),
        }
    }

    // 如果是图片,尝试删除对应的缩略图
    if app_state.conf.thumb_enabled {
        let content_type = utils::guess_mime_type(&local_path);
        if utils::is_image(&content_type) {
            let thumb_path = make_thumb_path(&app_state.conf.thumb_root, path);

            let _lock = app_state.thumbs_lock.lock().unwrap();
            match std::fs::remove_file(&thumb_path) {
                Ok(_) => debug!("thumb file \"{}\" removed", thumb_path.display()),
                Err(e) => debug!(
                    "remove thumb file \"{}\" failed, {}",
                    thumb_path.display(),
                    e
                ),
            }
        }
    }
    (StatusCode::OK, path.to_string()).into_response()
}

async fn fallback_to_dir_delete<P: AsRef<Path> + Copy>(
    app_state: Arc<AppState>,
    local_path: P,
    path: &str,
) -> Response {
    {
        let _lock = app_state.fs_lock.lock().unwrap();

        match std::fs::remove_dir_all(&local_path) {
            Ok(_) => info!("dir \"{}\" removed", local_path.as_ref().display()),
            Err(e) => warn!(
                "dir remove \"{}\" failed: {}",
                local_path.as_ref().display(),
                e
            ),
        }
    }

    if app_state.conf.thumb_enabled {
        // 删除对应的 thumb 文件夹(可能不存在)
        let thumb_path =
            PathBuf::from(&app_state.conf.thumb_root).join(path.strip_prefix("/").unwrap_or(path));

        let _lock = app_state.thumbs_lock.lock().unwrap();
        match std::fs::remove_dir_all(&thumb_path) {
            Ok(_) => debug!("thumb dir \"{}\" removed", thumb_path.display()),
            Err(e) => debug!(
                "thumb dir remove \"{}\" failed: {}",
                thumb_path.display(),
                e
            ),
        }
    }
    (StatusCode::OK, path.to_string()).into_response()
}

fn make_thumb_path(thumb_root: &str, path: &str) -> PathBuf {
    let mut thumb_path = PathBuf::from(thumb_root).join(path.strip_prefix("/").unwrap_or(path));
    thumb_path.add_extension("thumb");
    thumb_path
}

async fn fallback_to_image_thumb<P: AsRef<Path> + Copy>(
    app_state: Arc<AppState>,
    image_path: P,
    path: &str,
) -> Response {
    // 生成 thumb path: thumbs_root + path + .png
    let thumb_path = make_thumb_path(&app_state.conf.thumb_root, path);
    debug!("transfer to thumb \"{}\" ...", thumb_path.display());

    // 锁内判断缓存文件是否存在,存在则返回,否则启动生成 task
    let (tx, rx) = {
        let mut thumbs_waiting_queue = app_state.thumbs_lock.lock().unwrap();

        // 缩略图缓存目录是配置文件指定的,要求专用于 LwNAS,只需简单判断是否存在即可,不判断 is_file/is_dir 等
        if thumb_path.exists() {
            (None, None)
        } else {
            if let Some(tx) = thumbs_waiting_queue.get(path) {
                // 队列已经存在,说明生成任务正在运行,加入等待队列
                (None, Some(tx.subscribe()))
            } else {
                // 建立等待队列,并在生成完成时负责清理, broadcast channel 只需 1 个消息即可
                let (tx, rx) = broadcast::channel(1);

                // 经过 get 确认, should be none
                let _ = thumbs_waiting_queue.insert(path.to_string(), tx.clone());

                (Some(tx), Some(rx))
            }
        }
    };

    // 启动生成任务
    if let Some(tx) = tx {
        let perm = app_state
            .thumbs_max_parallel_sem
            .clone()
            .acquire_owned()
            .await
            .expect("thumbs_max_parallel_sem acquire");
        let app_state = app_state.clone();
        let path = path.to_string();
        let image_path = image_path.as_ref().to_path_buf();
        let thumb_path = thumb_path.clone();

        tokio::task::spawn_blocking(move || {
            // 生成任务期间持有 Semarphore 许可
            let _perm = perm;

            match utils::generate_thumbnail(image_path, app_state.conf.thumb_size) {
                Ok(thumb_data) => {
                    let r = {
                        // 必须在锁内
                        let mut thumbs_waiting_queue = app_state.thumbs_lock.lock().unwrap();

                        // 移除缓存生成状态记录
                        thumbs_waiting_queue
                            .remove(&path)
                            .expect("should not panic");

                        // 尝试创建目录,忽略结果: 可能已经存在,忽略; 失败,同样会导致后续写入失败.
                        let mut thumb_path_dir = thumb_path.clone();
                        thumb_path_dir.pop();
                        let _ = std::fs::create_dir_all(&thumb_path_dir);

                        // 写入磁盘
                        match std::fs::write(&thumb_path, &thumb_data) {
                            Ok(_) => {
                                debug!("thumb file \"{}\" saved", thumb_path.display());
                                true
                            }
                            Err(e) => {
                                warn!("save thumb file \"{}\"failed, {}", thumb_path.display(), e);
                                false
                            }
                        }
                    };

                    // 广播成功通知
                    let _ = tx.send(r);
                }
                Err(e) => {
                    warn!(
                        "generate thumb file \"{}\"failed, {}",
                        thumb_path.display(),
                        e
                    );
                    let _ = tx.send(false);
                }
            };
        });
    }

    // 等待生成
    if let Some(mut rx) = rx {
        // 生成完成
        debug!("thumb \"{}\" miss, waiting ...", thumb_path.display());
        match rx.recv().await {
            Ok(r) => {
                if r {
                    debug!("thumb \"{}\" ready", thumb_path.display());
                } else {
                    debug!("thumb generate failed");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            }
            Err(e) => {
                debug!("broadcast recv failed, {}", e);
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    } else {
        // 缓存命中
        debug!("thumb \"{}\" hit", thumb_path.display());
    }

    // 从磁盘缓存中读取缩略图
    let res = match FileStream::<ReaderStream<File>>::from_path(&thumb_path).await {
        Ok(file_stream) => file_stream.into_response(),
        _ => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    // thumbnail 固定生成 PNG 格式
    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("image/png"),
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_static("inline"),
    );

    (StatusCode::OK, h, res).into_response()
}
