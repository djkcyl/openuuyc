use super::{
    service::{Direction, PartialFile, Record, Repository, TaskState},
    storage, *,
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub(super) struct Rpc {
    pub id: i32,
    pub wire: Arc<Transport>,
    rx: mpsc::Receiver<Incoming>,
    _route: RouteGuard,
    pub stop: CancellationToken,
    fault: Arc<Mutex<Option<String>>>,
}
impl Rpc {
    pub fn new(wire: Arc<Transport>, stop: CancellationToken) -> Result<Self> {
        let fault = Arc::new(Mutex::new(None));
        let (id, rx, route) = wire.register(stop.clone(), Arc::clone(&fault))?;
        Ok(Self {
            id,
            wire,
            rx,
            _route: route,
            stop,
            fault,
        })
    }
    pub fn task(&self, index: i32) -> Option<TaskId> {
        Some(TaskId {
            task_id: self.id,
            file_index: index,
        })
    }
    pub fn check(&self, id: &Option<TaskId>, index: Option<i32>) -> Result<i32> {
        let id = id.as_ref().context("文件消息缺少任务编号")?;
        ensure!(
            id.task_id == self.id && index.is_none_or(|v| v == id.file_index),
            "文件任务或文件序号不匹配"
        );
        Ok(id.file_index)
    }
    pub async fn req(&self, v: Req, file: bool) -> Result<()> {
        ensure!(!self.stop.is_cancelled(), "{}", self.failure());
        // SCTP reserves capacity before packetization and commits a whole
        // message without suspension. Cancelling a pending send leaves no SSN gap.
        tokio::select! {
            biased;
            _ = self.stop.cancelled() => anyhow::bail!(self.failure()),
            result = self.wire.request(self.id, v, file) => result,
        }
    }
    pub async fn res(&self, v: Res) -> Result<()> {
        ensure!(!self.stop.is_cancelled(), "{}", self.failure());
        tokio::select! {
            biased;
            _ = self.stop.cancelled() => anyhow::bail!(self.failure()),
            result = self.wire.response(self.id, v) => result,
        }
    }
    fn failure(&self) -> String {
        lock(&self.fault)
            .clone()
            .unwrap_or_else(|| "任务已暂停".into())
    }
    pub async fn next(&mut self) -> Result<Incoming> {
        tokio::select! {biased;_=self.stop.cancelled()=>anyhow::bail!(self.failure()),r=tokio::time::timeout(Duration::from_secs(60),self.rx.recv())=>r.context("远端未响应，操作结果未确认")?.context("文件连接已结束")}
    }
    async fn complete(&self, index: i32, error: i32) -> Result<()> {
        self.req(
            Req::Complete(FileTransferComplete {
                id: self.task(index),
                error,
            }),
            false,
        )
        .await
    }
    async fn result(&mut self, index: i32) -> Result<()> {
        match self.next().await?.payload {
            Payload::Response(Res::Result(v)) => {
                self.check(&v.id, Some(index))?;
                success(v.file_error)
            }
            _ => anyhow::bail!("文件完成响应类型不匹配"),
        }
    }
}
pub(super) async fn directory(
    wire: Arc<Transport>,
    path: String,
    stop: CancellationToken,
) -> Result<Vec<FileEntry>> {
    let mut rpc = Rpc::new(wire, stop)?;
    rpc.req(
        Req::ReadDir(ReadDir {
            id: rpc.task(-1),
            path,
        }),
        false,
    )
    .await?;
    match rpc.next().await?.payload {
        Payload::Response(Res::FileDirectory(v)) => {
            rpc.check(&v.id, None)?;
            success(v.file_error)?;
            let mut entries = if v.dir_data.is_empty() {
                v.entries
            } else {
                storage::decompress::<DirectoryData>(&v.dir_data)?.entries
            };
            ensure!(entries.len() <= storage::MAX_FILES, "远端目录项目过多");
            ensure!(
                entries.iter().all(|e| e.name.len() < 65536
                    && e.full_path.len() < 65536
                    && !e.full_path.contains('\0')),
                "远端目录路径无效"
            );
            entries.sort_by(|a, b| {
                (a.entry_type >= 4, a.name.to_lowercase())
                    .cmp(&(b.entry_type >= 4, b.name.to_lowercase()))
            });
            Ok(entries)
        }
        _ => anyhow::bail!("目录响应类型不匹配"),
    }
}
pub(super) async fn operation(
    wire: Arc<Transport>,
    req: Req,
    stop: CancellationToken,
) -> Result<String> {
    let mut rpc = Rpc::new(wire, stop)?;
    let req = match req {
        Req::RemoveDir(mut v) => {
            v.id = rpc.task(-1);
            Req::RemoveDir(v)
        }
        v => v,
    };
    rpc.req(req, false).await?;
    match rpc.next().await?.payload {
        Payload::Response(Res::OperationResult(v)) => {
            success(v.file_error).with_context(|| format!("文件操作失败：{}", v.err_msg))?;
            Ok(v.path)
        }
        Payload::Response(Res::Result(v)) => {
            rpc.check(&v.id, None)?;
            success(v.file_error)?;
            Ok(String::new())
        }
        _ => anyhow::bail!("文件操作响应类型不匹配"),
    }
}
async fn exists(
    wire: Arc<Transport>,
    path: String,
    name: String,
    stop: CancellationToken,
) -> Result<(bool, bool)> {
    let mut rpc = Rpc::new(wire, stop)?;
    rpc.req(
        Req::FileExist(FileExist {
            path,
            names: vec![name.clone()],
        }),
        false,
    )
    .await?;
    match rpc.next().await?.payload {
        Payload::Response(Res::FileExistResponse(v)) => {
            let r = v
                .results
                .iter()
                .find(|r| r.name == name)
                .context("远端未返回同名检查结果")?;
            Ok((r.has_same, r.has_transfering))
        }
        _ => anyhow::bail!("同名检查响应类型不符"),
    }
}
pub(super) async fn create_directory(
    wire: Arc<Transport>,
    parent: String,
    name: String,
    stop: CancellationToken,
) -> Result<String> {
    ensure!(
        storage::safe_relative(&name)?.components().count() == 1,
        "文件夹名称无效"
    );
    // The official request takes a parent. The host allocates a unique new-folder name.
    let created = operation(
        wire.clone(),
        Req::DirCreate(FileDirCreate {
            path: parent.clone(),
        }),
        stop.clone(),
    )
    .await?;
    let normalized = created.replace('\\', "/");
    let (actual_parent, actual_name) = normalized
        .rsplit_once('/')
        .context("被控端未返回新目录路径")?;
    ensure!(
        actual_parent
            .trim_end_matches('/')
            .eq_ignore_ascii_case(&parent.replace('\\', "/").trim_end_matches('/')),
        "被控端返回的新目录不在指定位置"
    );
    ensure!(
        storage::safe_relative(actual_name)?.components().count() == 1,
        "被控端返回的新目录名称无效"
    );
    let target = super::service::join_remote(&parent, &name);
    if actual_name != name {
        operation(
            wire,
            Req::Rename(FileRename {
                path: created.clone(),
                new_name: target.clone(),
            }),
            stop,
        )
        .await
        .with_context(|| format!("已创建 {created}，但重命名未确认成功"))?;
    }
    Ok(target)
}
pub(super) async fn run(
    wire: Arc<Transport>,
    mut record: Record,
    repo: Arc<Repository>,
    stop: CancellationToken,
) {
    let outcome = async {
        let mut rpc = Rpc::new(wire.clone(), stop.clone())?;
        record.state = TaskState::Running;
        record.error = None;
        repo.save(&record)?;
        let result = match record.direction {
            Direction::Upload => upload(&mut rpc, &mut record, &repo).await,
            Direction::Download => download(&mut rpc, &mut record, &repo).await,
        };
        if result.is_err() {
            let code = if stop.is_cancelled() { 10 } else { 6 };
            // One stop notification. Failure/timeout is not permission to replay a command.
            let sent = wire
                .request(
                    rpc.id,
                    Req::Complete(FileTransferComplete {
                        id: rpc.task(-1),
                        error: code,
                    }),
                    false,
                )
                .await;
            if stop.is_cancelled() && sent.is_ok() && wire.supported() {
                // Drain in-flight block acknowledgements until the host acknowledges
                // pause; do not reopen the same persisted task while it is stopping.
                let _ = tokio::time::timeout(Duration::from_secs(3), async {
                    while let Some(message) = rpc.rx.recv().await {
                        if matches!(
                            message.payload,
                            Payload::Response(Res::Result(FileTransferResult {
                                file_error: 10,
                                ..
                            }))
                        ) {
                            break;
                        }
                    }
                })
                .await;
            }
        }
        result
    }
    .await;
    record.state = if outcome.is_ok() {
        if record.state == TaskState::Skipped {
            TaskState::Skipped
        } else {
            TaskState::Done
        }
    } else if stop.is_cancelled() {
        TaskState::Paused
    } else {
        TaskState::Failed
    };
    record.error = if stop.is_cancelled() && wire.supported() && wire.allowed() {
        None
    } else {
        outcome.err().map(|e| format!("{e:#}"))
    };
    record.confirmed = record
        .confirmed
        .max(repo.transferred(&record.key))
        .max(record.completed_bytes());
    if let Err(e) = repo.save(&record) {
        repo.report(format!("无法保存续传记录：{e:#}"));
    }
    repo.progress(&record.key, record.confirmed, true);
}
async fn upload(rpc: &mut Rpc, r: &mut Record, repo: &Repository) -> Result<()> {
    if !r.initialized {
        let source = PathBuf::from(&r.source);
        let stop = rpc.stop.clone();
        let (root, folder, files) =
            tokio::task::spawn_blocking(move || storage::scan(&source, &stop)).await??;
        r.local_root = Some(root);
        r.folder = folder;
        r.files = files;
        r.initialized = true;
        r.total = storage::validate_manifest(&r.files)?;
        repo.save(r)?;
    }
    let root = r.local_root.clone().context("本地源目录无效")?;
    if !r.started {
        let name = if r.folder.is_empty() {
            r.files
                .first()
                .context("没有要发送的文件")?
                .rel_path
                .clone()
        } else {
            r.folder.clone()
        };
        let (same, transferring) = exists(
            rpc.wire.clone(),
            r.destination.clone(),
            name,
            rpc.stop.clone(),
        )
        .await?;
        ensure!(!transferring, "另一个任务正在写入同名文件，请等待完成");
        if same && r.policy == 3 {
            r.state = TaskState::Skipped;
            return Ok(());
        }
    }
    let files = r
        .files
        .iter()
        .filter(|f| {
            !r.partial
                .iter()
                .any(|p| p.info.rel_path == f.rel_path && (p.done || p.skipped))
        })
        .cloned()
        .collect::<Vec<_>>();
    if r.started && files.is_empty() {
        return Ok(());
    }
    let list = storage::compress(&FileList {
        files: files.clone(),
    })?;
    rpc.req(
        Req::ReceiveRequest(FileTransferReceiveRequest {
            id: rpc.task(-1),
            path: r.destination.clone(),
            files: vec![],
            file_op_strategy: if r.started { 4 } else { r.policy },
            folder_name: r.folder.clone(),
            list_data: list,
            task_unique_id: r.key.clone(),
        }),
        false,
    )
    .await?;
    r.started = true;
    repo.save(r)?;
    let skipped = match rpc.next().await.context("等待上传清单响应")?.payload {
        Payload::Response(Res::ReceiveResponse(v)) => {
            rpc.check(&v.id, None)?;
            if matches!(v.err, 4 | 8 | 12) {
                r.started = false;
            }
            success(v.err).context("远端拒绝接收文件清单")?;
            v.files
                .into_iter()
                .map(|f| f.rel_path.replace('\\', "/"))
                .collect::<HashSet<_>>()
        }
        _ => anyhow::bail!("上传初始化响应类型不匹配"),
    };
    let mut done = r.completed_bytes();
    for index in 0..files.len() {
        ensure!(!rpc.stop.is_cancelled(), "任务已暂停");
        let info = files[index].clone();
        if r.policy == 3 && skipped.contains(&info.rel_path.replace('\\', "/")) {
            done += info.size;
            upsert(
                r,
                PartialFile {
                    target: info.rel_path.clone(),
                    info: info.clone(),
                    done: false,
                    skipped: true,
                },
            );
            repo.save(r)?;
            repo.progress(&r.key, done, false);
            continue;
        }
        rpc.req(
            Req::FileAsk(FileTransferAsk {
                id: rpc.task(index as i32 + 1),
                last_modified: info.modified_time,
                file_size: info.size,
            }),
            false,
        )
        .await?;
        let confirm = match rpc.next().await.context("等待文件续传位置")?.payload {
            Payload::Response(Res::FileConfirm(v)) => v,
            _ => anyhow::bail!("文件准备响应类型不匹配"),
        };
        rpc.check(&confirm.id, Some(index as i32 + 1))?;
        if confirm.skip {
            success(confirm.err)?;
            done += info.size;
            upsert(
                r,
                PartialFile {
                    target: info.rel_path.clone(),
                    info: info.clone(),
                    done: false,
                    skipped: true,
                },
            );
            repo.save(r)?;
            repo.progress(&r.key, done, false);
            continue;
        }
        success(confirm.err).with_context(|| format!("远端无法准备文件 {}", info.rel_path))?;
        ensure!(confirm.resume_point <= info.size, "远端续传位置超过源文件");
        let source_root = root.clone();
        let item = info.clone();
        let file = tokio::task::spawn_blocking(move || storage::open_source(&source_root, &item))
            .await??;
        let mut file = tokio::fs::File::from_std(file);
        file.seek(std::io::SeekFrom::Start(confirm.resume_point))
            .await?;
        let mut pos = confirm.resume_point;
        let mut accepted = confirm.resume_point;
        let mut block = 0i32;
        let mut pending = HashMap::<i32, usize>::new();
        while pos < info.size || !pending.is_empty() {
            if pos < info.size && pending.len() < 16 {
                let size = BLOCK.min((info.size - pos) as usize);
                let mut data = vec![0; size];
                tokio::select! {biased;_=rpc.stop.cancelled()=>anyhow::bail!("任务已暂停"),v=file.read_exact(&mut data)=>{v.context("源文件读取失败")?;}}
                block = block.checked_add(1).context("文件块编号超出范围")?;
                rpc.req(
                    Req::FileBlock(FileTransferBlock {
                        id: rpc.task(index as i32 + 1),
                        block_id: block,
                        data,
                    }),
                    true,
                )
                .await?;
                pending.insert(block, size);
                pos += size as u64;
                if pending.len() < 16 && pos < info.size {
                    continue;
                }
            }
            match rpc.next().await.context("等待文件块确认")?.payload {
                Payload::Response(Res::BlockConfirm(v)) => {
                    rpc.check(&v.id, Some(index as i32 + 1))?;
                    success(v.err)?;
                    let len = pending
                        .remove(&v.block_id)
                        .context("收到重复或未知的文件块确认")?;
                    ensure!(
                        v.block_len >= 0 && v.block_len as usize == len,
                        "文件块确认长度不符"
                    );
                    accepted += len as u64;
                    repo.progress(&r.key, done + accepted, false);
                }
                _ => anyhow::bail!("上传文件块响应类型不匹配"),
            }
        }
        drop(file);
        rpc.complete(index as i32 + 1, 1).await?;
        rpc.result(index as i32 + 1)
            .await
            .context("远端未确认单文件保存成功")?;
        upsert(
            r,
            PartialFile {
                target: info.rel_path.clone(),
                info: info.clone(),
                done: true,
                skipped: false,
            },
        );
        repo.save(r)?;
        done += info.size;
        repo.progress(&r.key, done, false);
    }
    rpc.complete(-1, 1).await?;
    // The host retires the receiver after the last per-file result. The final
    // whole-task notification has no additional reply on that path.
    Ok(())
}
async fn download(rpc: &mut Rpc, r: &mut Record, repo: &Repository) -> Result<()> {
    if !r.started {
        let name = r
            .source
            .trim_end_matches(['\\', '/'])
            .rsplit(['\\', '/'])
            .next()
            .context("远端源文件无效")?;
        let relative = storage::safe_relative(name)?;
        if r.policy == 3 && PathBuf::from(&r.destination).join(relative).exists() {
            r.state = TaskState::Skipped;
            return Ok(());
        }
    }
    let remaining = r
        .files
        .iter()
        .filter(|f| {
            !r.partial
                .iter()
                .any(|p| p.info.rel_path == f.rel_path && (p.done || p.skipped))
        })
        .cloned()
        .collect::<Vec<_>>();
    if r.started && remaining.is_empty() {
        return Ok(());
    }
    let list_data = if r.initialized {
        storage::compress(&FileList {
            files: remaining.clone(),
        })?
    } else {
        vec![]
    };
    rpc.req(
        Req::SendRequest(FileTransferSendRequest {
            id: rpc.task(-1),
            path: r.source.clone(),
            list_data,
        }),
        false,
    )
    .await?;
    let response = loop {
        match rpc.next().await?.payload {
            Payload::Response(Res::SendResponse(v)) => break v,
            Payload::Request(Req::DirSizeRead(v)) => {
                rpc.check(&v.id, None)?;
                r.total = v.size;
            }
            _ => anyhow::bail!("下载初始化响应类型不匹配"),
        }
    };
    rpc.check(&response.id, None)?;
    success(response.err)?;
    let files = if response.list_data.is_empty() {
        response.files
    } else {
        storage::decompress::<FileList>(&response.list_data)?.files
    };
    r.total = storage::validate_manifest(&files)?
        .checked_add(r.completed_bytes())
        .context("任务大小溢出")?;
    if r.initialized {
        ensure!(
            remaining == files && r.folder == response.folder_name,
            "源文件清单已变化，不能续传原任务"
        );
    }
    r.files = files;
    r.folder = response.folder_name;
    r.initialized = true;
    if r.local_root.is_none() {
        let dest = PathBuf::from(&r.destination);
        let folder = r.folder.clone();
        let policy = r.policy;
        r.local_root = Some(
            tokio::task::spawn_blocking(move || -> Result<PathBuf> {
                let root = storage::canonical_dir(&dest)?;
                if folder.is_empty() {
                    return Ok(root);
                }
                let relative = storage::safe_relative(&folder)?;
                ensure!(relative.components().count() == 1, "远端文件夹名包含路径");
                let mut target = root.join(&relative);
                if target.exists() && policy == 2 {
                    for i in 1..10000 {
                        target = root.join(format!("{folder} ({i})"));
                        if !target.exists() {
                            break;
                        }
                    }
                    ensure!(!target.exists(), "无法分配目标目录");
                }
                if !target.exists() {
                    std::fs::create_dir(&target)?;
                }
                storage::canonical_dir(&target)
            })
            .await??,
        );
    }
    let root = r.local_root.clone().context("下载目录无效")?;
    r.started = true;
    repo.save(r)?;
    // A zero-file directory is created locally; the server still completes its task.
    let mut open: Option<(i32, storage::Receiving, i32)> = None;
    let mut completed = HashSet::<i32>::new();
    loop {
        let incoming = rpc.next().await?;
        match incoming.payload {
            Payload::Request(Req::FileAsk(v)) => {
                ensure!(open.is_none(), "远端尚未完成当前文件");
                let index = rpc.check(&v.id, None)?;
                ensure!(
                    index > 0 && !completed.contains(&index),
                    "远端重复或无效文件序号"
                );
                let info = r
                    .files
                    .get((index - 1) as usize)
                    .context("远端文件序号越界")?
                    .clone();
                ensure!(
                    info.size == v.file_size && info.modified_time == v.last_modified,
                    "远端文件在传输前已变化"
                );
                let old = r
                    .partial
                    .iter()
                    .find(|p| p.info.rel_path == info.rel_path)
                    .cloned();
                let dir = root.clone();
                let key = r.key.clone();
                let policy = r.policy;
                let item = info.clone();
                let prepared = tokio::task::spawn_blocking(move || {
                    storage::prepare(&dir, &key, &item, policy, old.as_ref())
                })
                .await??;
                let skip = prepared.is_none();
                let offset = prepared.as_ref().map_or(0, |p| p.position);
                if let Some(p) = &prepared {
                    upsert(r, p.partial.clone());
                } else if !r.partial.iter().any(|p| p.info.rel_path == info.rel_path) {
                    upsert(
                        r,
                        PartialFile {
                            target: info.rel_path.clone(),
                            info,
                            done: false,
                            skipped: true,
                        },
                    );
                }
                repo.save(r)?;
                if let Some(mut p) = prepared {
                    p.file.seek(std::io::SeekFrom::Start(offset)).await?;
                    open = Some((index, p, 0));
                }
                if skip {
                    completed.insert(index);
                }
                rpc.res(Res::FileConfirm(FileTransferConfirm {
                    id: rpc.task(index),
                    skip,
                    err: 1,
                    resume_point: offset,
                }))
                .await?;
            }
            Payload::Request(Req::FileBlock(v)) => {
                let (index, p, last) = open.as_mut().context("尚未准备接收文件")?;
                rpc.check(&v.id, Some(*index))?;
                ensure!(
                    v.data.len() < WIRE
                        && v.block_id == last.checked_add(1).context("文件块编号溢出")?,
                    "文件块顺序或长度无效"
                );
                ensure!(
                    p.position
                        .checked_add(v.data.len() as u64)
                        .is_some_and(|n| n <= p.partial.info.size),
                    "文件数据超出声明长度"
                );
                p.file.write_all(&v.data).await?;
                p.position += v.data.len() as u64;
                *last = v.block_id;
                repo.progress(&r.key, r.completed_bytes() + p.position, false);
                rpc.res(Res::BlockConfirm(FileTransferBlockConfirm {
                    id: rpc.task(*index),
                    block_id: v.block_id,
                    err: 1,
                    block_len: v.data.len() as i32,
                }))
                .await?;
            }
            Payload::Request(Req::Complete(v)) => {
                let index = rpc.check(&v.id, None)?;
                if v.error != 1 {
                    success(v.error)?;
                }
                if index == -1 {
                    ensure!(
                        open.is_none() && (r.files.is_empty() || completed.len() == r.files.len()),
                        "远端提前结束文件任务"
                    );
                    rpc.res(Res::Result(FileTransferResult {
                        id: rpc.task(-1),
                        file_error: 1,
                        err_msg: String::new(),
                    }))
                    .await?;
                    return Ok(());
                }
                let (active, file, _) = open.take().context("远端完成了未打开的文件")?;
                ensure!(index == active, "完成文件序号不符");
                let partial = storage::finish(file, &root, &r.key, r.policy).await?;
                upsert(r, partial);
                repo.save(r)?;
                completed.insert(index);
                rpc.res(Res::Result(FileTransferResult {
                    id: rpc.task(index),
                    file_error: 1,
                    err_msg: String::new(),
                }))
                .await?;
                repo.progress(&r.key, r.completed_bytes(), false);
            }
            Payload::Request(Req::DirSizeRead(v)) => {
                rpc.check(&v.id, None)?;
            }
            _ => anyhow::bail!("下载消息与当前任务状态不匹配"),
        }
    }
}
fn upsert(r: &mut Record, p: PartialFile) {
    if let Some(old) = r
        .partial
        .iter_mut()
        .find(|o| o.info.rel_path == p.info.rel_path)
    {
        *old = p;
    } else {
        r.partial.push(p);
    }
}
