use crate::{
    cache::{self, CacheAction, CleanupCandidate},
    cargo::{self, RustProject},
    index::{self, Query, Sort},
    model::*,
    operations::{self, Action, ConflictPolicy, OperationPlan},
    scan::{self, ScanHandle, ScanRequest},
    settings::{AppPaths, Settings},
};
use crossbeam_channel::{Receiver, Sender, bounded};
use eframe::egui::{self, Color32, RichText};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::BufRead,
    path::PathBuf,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Files,
    Caches,
    Rust,
    History,
    Settings,
}
enum Event {
    Rows(u64, Result<Vec<FileEntry>, String>),
    Cache(CleanupCandidate),
    Rust(RustProject),
    Result(OperationResult),
    Done,
    Error(String),
    History(Vec<OperationResult>),
    ScanErrors(Vec<(String, String)>),
    StartupNotice(String),
}
#[derive(Clone)]
enum Pending {
    Files(OperationPlan),
    Caches(Vec<CleanupCandidate>),
    Rust(Vec<RustProject>),
}
pub struct App {
    paths: AppPaths,
    settings: Settings,
    page: Page,
    scan: Option<ScanHandle>,
    progress: ScanProgress,
    task: Option<Cancellation>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    query: Query,
    query_id: u64,
    query_running: bool,
    query_dirty: bool,
    rows: Vec<FileEntry>,
    selected: HashMap<PathBuf, FileEntry>,
    caches: Vec<CleanupCandidate>,
    cache_selected: HashSet<usize>,
    projects: Vec<RustProject>,
    rust_selected: HashSet<usize>,
    history: VecDeque<OperationResult>,
    pending: Option<Pending>,
    notice: String,
    root_input: String,
    exclusion_input: String,
    move_destination: String,
    conflict: ConflictPolicy,
    before_days: u32,
    last_query: Instant,
    confirmed: bool,
    scan_errors: Vec<(String, String)>,
    smoke_start: Option<Instant>,
    smoke_frames: Vec<f64>,
    task_error: bool,
}
impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> anyhow::Result<Self> {
        let paths = AppPaths::discover()?;
        paths.create()?;
        let (settings, notice) = match paths.load() {
            Ok(s) => (s, "添加目录并开始扫描，所有清理均由你确认。".into()),
            Err(e) => (
                Settings::default(),
                format!("设置读取失败，使用默认值：{e}"),
            ),
        };
        let (tx, rx) = bounded(64);
        let mut style = (*cc.egui_ctx.global_style()).clone();
        style.spacing.item_spacing = egui::vec2(10.0, 10.0);
        style.spacing.button_padding = egui::vec2(12.0, 8.0);
        cc.egui_ctx.set_global_style(style);
        let history_tx = tx.clone();
        let history_path = paths.data.join("history.jsonl");
        let startup_context = cc.egui_ctx.clone();
        thread::spawn(move || {
            let mut history = VecDeque::new();
            if let Ok(file) = std::fs::File::open(history_path) {
                for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
                    if let Ok(item) = serde_json::from_str(&line) {
                        if history.len() == 500 {
                            history.pop_front();
                        }
                        history.push_back(item);
                    }
                }
            }
            let _ = history_tx.send(Event::History(history.into()));
            let notice = match crate::shortcut::ensure() {
                Ok(crate::shortcut::Outcome::Created {
                    path,
                    needs_trust: true,
                }) => Some(format!(
                    "桌面快捷方式已创建：{}。若系统提示未信任，请右键选择“允许启动”。",
                    path.display()
                )),
                Ok(_) => None,
                Err(e) => Some(format!("无法自动创建桌面快捷方式：{e:#}。程序可继续使用。")),
            };
            if let Some(notice) = notice {
                let _ = history_tx.send(Event::StartupNotice(notice));
            }
            startup_context.request_repaint();
        });
        let has_index = paths.cache.join("scan.sqlite").exists();
        let mut app = Self {
            query: Query {
                min_bytes: settings.min_mib.saturating_mul(1024 * 1024),
                ..Default::default()
            },
            paths,
            settings,
            page: Page::Files,
            scan: None,
            progress: ScanProgress::default(),
            task: None,
            tx,
            rx,
            query_id: 0,
            query_running: false,
            query_dirty: has_index,
            rows: vec![],
            selected: HashMap::new(),
            caches: vec![],
            cache_selected: HashSet::new(),
            projects: vec![],
            rust_selected: HashSet::new(),
            history: VecDeque::new(),
            pending: None,
            notice,
            root_input: String::new(),
            exclusion_input: String::new(),
            move_destination: String::new(),
            conflict: ConflictPolicy::Skip,
            before_days: 0,
            last_query: Instant::now(),
            confirmed: false,
            scan_errors: vec![],
            smoke_start: std::env::var_os("RSCLEAR_SMOKE_EXIT").map(|_| Instant::now()),
            smoke_frames: Vec::new(),
            task_error: false,
        };
        if app.smoke_start.is_some()
            && let Some(root) = std::env::var_os("RSCLEAR_SMOKE_SCAN")
        {
            app.settings.roots = vec![PathBuf::from(root)];
            app.settings.min_mib = 0;
            app.query.min_bytes = 0;
            app.start_scan();
        }
        Ok(app)
    }
    fn busy(&self) -> bool {
        self.scan.is_some() || self.task.is_some()
    }
    fn database(&self) -> PathBuf {
        self.paths.cache.join("scan.sqlite")
    }
    fn poll(&mut self) {
        let mut finished = false;
        if let Some(scan) = &self.scan {
            while let Ok(event) = scan.events.try_recv() {
                match event {
                    ScanEvent::Progress(p) => {
                        self.progress = p;
                        if self.last_query.elapsed() > Duration::from_secs(1) {
                            self.query_dirty = true;
                        }
                    }
                    ScanEvent::Finished {
                        progress,
                        cancelled,
                    } => {
                        self.progress = progress;
                        self.notice = if cancelled {
                            "扫描已取消，保留已扫描结果。"
                        } else {
                            "扫描完成。选择文件后可预览操作。"
                        }
                        .into();
                        finished = true;
                    }
                    ScanEvent::Failed(e) => {
                        self.notice = e;
                        finished = true;
                    }
                }
            }
        }
        if finished {
            if let Some(scan) = self.scan.take() {
                let _ = scan.join.join();
            }
            self.query_dirty = true;
            let (tx, database) = (self.tx.clone(), self.database());
            thread::spawn(move || {
                let _ = tx.send(Event::ScanErrors(
                    index::errors(&database).unwrap_or_default(),
                ));
            });
        }
        for _ in 0..128 {
            let Ok(event) = self.rx.try_recv() else {
                break;
            };
            match event {
                Event::Rows(id, result) => {
                    self.query_running = false;
                    if id == self.query_id {
                        match result {
                            Ok(rows) => self.rows = rows,
                            Err(e) => self.notice = e,
                        }
                    }
                }
                Event::Cache(item) => self.caches.push(item),
                Event::Rust(item) => self.projects.push(item),
                Event::Result(item) => {
                    if self.history.len() == 500 {
                        self.history.pop_front();
                    }
                    self.history.push_back(item);
                }
                Event::Done => {
                    self.task = None;
                    if !self.task_error {
                        self.notice = "任务结束。请查看逐项结果；清理后重新扫描以刷新占用。".into();
                    }
                }
                Event::Error(error) => {
                    self.task_error = true;
                    self.notice = error;
                }
                Event::History(history) => self.history.extend(history),
                Event::ScanErrors(errors) => self.scan_errors = errors,
                Event::StartupNotice(notice) => self.notice = notice,
            }
        }
        if self.query_dirty
            && !self.query_running
            && self.database().exists()
            && self.last_query.elapsed() >= Duration::from_millis(200)
        {
            self.query_dirty = false;
            self.query_running = true;
            self.last_query = Instant::now();
            let (tx, database, query, id) = (
                self.tx.clone(),
                self.database(),
                self.query.clone(),
                self.query_id,
            );
            thread::spawn(move || {
                let _ = tx.send(Event::Rows(
                    id,
                    index::query(&database, &query).map_err(|e| e.to_string()),
                ));
            });
        }
    }
    fn refresh(&mut self) {
        self.query_id += 1;
        self.query_dirty = true;
    }
    fn start_scan(&mut self) {
        if self.busy() || self.query_running {
            return;
        }
        self.query_dirty = false;
        self.selected.clear();
        self.rows.clear();
        self.projects.clear();
        self.rust_selected.clear();
        self.scan_errors.clear();
        self.progress = ScanProgress::default();
        self.query.offset = 0;
        self.query_id += 1;
        let mut exclusions = self.settings.exclusions.clone();
        exclusions.extend([self.paths.cache.clone(), self.paths.data.clone()]);
        if let Some(base) = directories::BaseDirs::new() {
            exclusions.push(base.data_dir().join("Trash"));
            exclusions.push(base.home_dir().join(".Trash"));
        }
        self.scan = Some(scan::start(ScanRequest {
            roots: self.settings.roots.clone(),
            exclusions,
            threads: self.settings.threads,
            database: self.database(),
        }));
        self.notice = "正在扫描；扫描期间不执行文件修改。".into();
    }
    fn discover(&mut self, rust: bool) {
        if self.busy() {
            return;
        }
        self.task_error = false;
        if rust {
            self.projects.clear();
            self.rust_selected.clear();
        } else {
            self.caches.clear();
            self.cache_selected.clear();
        }
        let token = Cancellation::default();
        self.task = Some(token.clone());
        let (tx, database, days) = (
            self.tx.clone(),
            self.database(),
            self.settings.retention_days,
        );
        self.notice = "正在识别目录、统计占用…".into();
        thread::spawn(move || {
            let result = if rust {
                index::manifests(&database).map(|manifests| {
                    cargo::discover(manifests, &token, |p| {
                        let _ = tx.send(Event::Rust(p));
                    })
                })
            } else {
                cache::discover(days, &token, |p| {
                    let _ = tx.send(Event::Cache(p));
                })
            };
            if let Err(e) = result {
                let _ = tx.send(Event::Error(format!("{e:#}")));
            }
            let _ = tx.send(Event::Done);
        });
    }
    fn run_pending(&mut self, pending: Pending) {
        if self.busy() {
            return;
        }
        self.task_error = false;
        let token = Cancellation::default();
        self.task = Some(token.clone());
        let tx = self.tx.clone();
        let history = self.paths.data.join("history.jsonl");
        self.selected.clear();
        self.cache_selected.clear();
        self.rust_selected.clear();
        self.rows.clear();
        self.caches.clear();
        self.projects.clear();
        self.page = Page::History;
        self.notice = "正在执行；取消不会恢复已完成的删除。".into();
        thread::spawn(move || {
            let emit = |mut result: OperationResult| {
                if result.message.len() > 16384 {
                    result.message = result.message.chars().take(4096).collect::<String>()
                        + "\n（命令输出已截断）";
                }
                if let Err(e) = operations::append_history(&history, &result) {
                    let _ = tx.send(Event::Error(format!("任务结果无法保存：{e}")));
                }
                let _ = tx.send(Event::Result(result));
            };
            let result = |item: String, action: &dyn Fn() -> anyhow::Result<String>| {
                let outcome = if token.is_cancelled() {
                    Err(anyhow::anyhow!("未执行：任务已取消"))
                } else {
                    action()
                };
                let (status, message) = match outcome {
                    Ok(message) => (ItemStatus::Succeeded, message),
                    Err(e) => (
                        if token.is_cancelled() {
                            ItemStatus::Cancelled
                        } else {
                            ItemStatus::Failed
                        },
                        format!("{e:#}"),
                    ),
                };
                emit(OperationResult {
                    item,
                    status,
                    message,
                    processed_bytes: 0,
                });
            };
            match pending {
                Pending::Files(plan) => operations::execute(&plan, &token, emit),
                Pending::Caches(candidates) => {
                    for candidate in candidates {
                        result(candidate.path.display().to_string(), &|| {
                            cache::clean(&candidate, &token)
                        });
                    }
                }
                Pending::Rust(projects) => {
                    for project in projects {
                        result(project.target.display().to_string(), &|| {
                            cargo::clean(&project, &token)
                        });
                    }
                }
            }
            let _ = tx.send(Event::Done);
        });
    }
    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(match self.page {
                Page::Files => "大文件",
                Page::Caches => "缓存清理",
                Page::Rust => "Rust 项目",
                Page::History => "任务记录",
                Page::Settings => "设置",
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.busy() {
                    if ui.button("取消当前任务").clicked() {
                        if let Some(scan) = &self.scan {
                            scan.cancel.cancel();
                        }
                        if let Some(task) = &self.task {
                            task.cancel();
                        }
                    }
                    ui.spinner();
                }
            });
        });
        ui.label(RichText::new(&self.notice).color(Color32::from_rgb(115, 155, 190)));
        ui.add_space(6.0);
        ui.separator();
    }
    fn roots(&mut self, ui: &mut egui::Ui) {
        let mut remove = None;
        for (i, path) in self.settings.roots.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(path.display().to_string());
                if ui
                    .add_enabled(!self.busy(), egui::Button::new("移除"))
                    .clicked()
                {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.settings.roots.remove(i);
        }
        ui.horizontal(|ui| {
            ui.add_enabled(
                !self.busy(),
                egui::TextEdit::singleline(&mut self.root_input)
                    .hint_text("输入绝对目录路径")
                    .desired_width(330.0),
            );
            if ui
                .add_enabled(!self.busy(), egui::Button::new("添加目录"))
                .clicked()
                && !self.root_input.trim().is_empty()
            {
                self.settings
                    .roots
                    .push(PathBuf::from(self.root_input.trim()));
                self.root_input.clear();
            }
            if ui
                .add_enabled(!self.busy(), egui::Button::new("选择…"))
                .clicked()
                && let Some(path) = rfd::FileDialog::new().pick_folder()
            {
                self.settings.roots.push(path);
            }
            if ui
                .add_enabled(
                    !self.busy() && !self.query_running && !self.settings.roots.is_empty(),
                    egui::Button::new("开始扫描"),
                )
                .clicked()
            {
                self.start_scan();
            }
        });
    }
    fn files(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("扫描目录")
            .default_open(true)
            .show(ui, |ui| self.roots(ui));
        ui.horizontal(|ui| {
            ui.label(format!(
                "{} 个文件  ·  {} 个目录",
                self.progress.files, self.progress.directories
            ));
            ui.label(format!(
                "磁盘占用 {}  ·  {:.1}s  ·  {} 个错误",
                size_text(self.progress.allocated),
                self.progress.elapsed_ms as f64 / 1000.0,
                self.progress.errors
            ));
        });
        if !self.scan_errors.is_empty() {
            egui::CollapsingHeader::new("扫描错误（最多展示 100 条）").show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(100.0)
                    .show(ui, |ui| {
                        for (path, error) in &self.scan_errors {
                            ui.label(format!("{path}: {error}"));
                        }
                    });
            });
        }
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            changed |= ui
                .add(
                    egui::TextEdit::singleline(&mut self.query.search)
                        .hint_text("搜索路径或名称")
                        .desired_width(180.0),
                )
                .changed();
            ui.label("至少");
            changed |= ui
                .add(
                    egui::DragValue::new(&mut self.settings.min_mib)
                        .range(0..=10_000_000)
                        .suffix(" MiB"),
                )
                .changed();
            changed |= ui
                .add(
                    egui::TextEdit::singleline(&mut self.query.extension)
                        .hint_text("扩展名")
                        .desired_width(70.0),
                )
                .changed();
            ui.label("修改早于");
            changed |= ui
                .add(
                    egui::DragValue::new(&mut self.before_days)
                        .range(0..=3650)
                        .suffix(" 天"),
                )
                .changed();
            egui::ComboBox::from_id_salt("sort")
                .selected_text(if self.query.sort == Sort::Size {
                    "大小降序"
                } else {
                    "修改时间降序"
                })
                .show_ui(ui, |ui| {
                    changed |= ui
                        .selectable_value(&mut self.query.sort, Sort::Size, "大小降序")
                        .changed();
                    changed |= ui
                        .selectable_value(&mut self.query.sort, Sort::Modified, "修改时间降序")
                        .changed();
                });
        });
        if changed {
            self.query.min_bytes = self.settings.min_mib.saturating_mul(1024 * 1024);
            self.query.modified_before =
                (self.before_days > 0).then(|| now() - i64::from(self.before_days) * 86400);
            self.query.offset = 0;
            self.refresh();
        }
        ui.horizontal(|ui| {
            if ui.button("选择本页").clicked() {
                for entry in &self.rows {
                    if self.selected.len() < 10_000 {
                        self.selected.insert(entry.path.clone(), entry.clone());
                    }
                }
            }
            if ui.button("取消选择").clicked() {
                self.selected.clear();
            }
            ui.label(format!("已选 {} 项", self.selected.len()));
            if ui
                .add_enabled(
                    !self.busy() && !self.selected.is_empty(),
                    egui::Button::new("移入废纸篓…"),
                )
                .clicked()
            {
                self.pending = Some(Pending::Files(OperationPlan {
                    entries: self.selected.values().cloned().collect(),
                    action: Action::Trash,
                }));
                self.confirmed = false;
            }
            if ui
                .add_enabled(
                    !self.busy() && !self.selected.is_empty(),
                    egui::Button::new("移动到…"),
                )
                .clicked()
                && let Some(destination) = rfd::FileDialog::new().pick_folder()
            {
                self.move_destination = destination.display().to_string();
                self.pending = Some(Pending::Files(OperationPlan {
                    entries: self.selected.values().cloned().collect(),
                    action: Action::Move {
                        destination,
                        conflict: self.conflict,
                    },
                }));
                self.confirmed = false;
            }
        });
        egui::CollapsingHeader::new("输入移动目标目录").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.move_destination)
                        .hint_text("目标目录的绝对路径")
                        .desired_width(360.0),
                );
                if ui
                    .add_enabled(
                        !self.busy() && !self.selected.is_empty(),
                        egui::Button::new("预览移动…"),
                    )
                    .clicked()
                {
                    let destination = PathBuf::from(self.move_destination.trim());
                    if destination.is_absolute() {
                        self.pending = Some(Pending::Files(OperationPlan {
                            entries: self.selected.values().cloned().collect(),
                            action: Action::Move {
                                destination,
                                conflict: self.conflict,
                            },
                        }));
                        self.confirmed = false;
                    } else {
                        self.notice = "请输入目标目录的绝对路径。".into();
                    }
                }
            });
        });
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    self.query.offset > 0 && !self.query_running,
                    egui::Button::new("上一页"),
                )
                .clicked()
            {
                self.query.offset = self.query.offset.saturating_sub(index::PAGE_SIZE as u64);
                self.refresh();
            }
            ui.label(format!(
                "第 {} 页 · 每页 {} 项",
                self.query.offset / index::PAGE_SIZE as u64 + 1,
                index::PAGE_SIZE
            ));
            if ui
                .add_enabled(
                    self.rows.len() == index::PAGE_SIZE && !self.query_running,
                    egui::Button::new("下一页"),
                )
                .clicked()
            {
                self.query.offset += index::PAGE_SIZE as u64;
                self.refresh();
            }
            if self.query_running {
                ui.spinner();
            }
        });
        egui::ScrollArea::vertical().id_salt("files").show_rows(
            ui,
            72.0,
            self.rows.len(),
            |ui, range| {
                for i in range {
                    let entry = &self.rows[i];
                    let mut checked = self.selected.contains_key(&entry.path);
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut checked, "").changed() {
                            if checked {
                                if self.selected.len() < 10_000 {
                                    self.selected.insert(entry.path.clone(), entry.clone());
                                }
                            } else {
                                self.selected.remove(&entry.path);
                            }
                        }
                        ui.vertical(|ui| {
                            ui.set_max_width((ui.available_width() - 185.0).max(240.0));
                            ui.horizontal(|ui| {
                                ui.strong(
                                    entry.path.file_name().unwrap_or_default().to_string_lossy(),
                                );
                                ui.label(format!(
                                    "{} · 占用 {} · 修改于约 {} 天前",
                                    size_text(entry.fingerprint.size),
                                    size_text(entry.allocated),
                                    (now() - entry.fingerprint.mtime).max(0) / 86400
                                ));
                            });
                            ui.add(
                                egui::Label::new(
                                    RichText::new(entry.path.display().to_string()).small(),
                                )
                                .truncate(),
                            )
                            .on_hover_text(entry.path.display().to_string());
                        });
                        if ui.small_button("复制路径").clicked() {
                            ui.ctx().copy_text(entry.path.display().to_string());
                        }
                        if ui.small_button("定位").clicked()
                            && let Err(e) = crate::platform::reveal(&entry.path)
                        {
                            self.notice = e.to_string();
                        }
                    });
                    ui.separator();
                }
            },
        );
        if self.rows.is_empty() {
            ui.label("暂无匹配结果。开始扫描或调整筛选条件。");
        }
    }
    fn caches(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.busy(), egui::Button::new("识别缓存"))
                .clicked()
            {
                self.discover(false);
            }
            if ui
                .add_enabled(
                    !self.busy() && !self.cache_selected.is_empty(),
                    egui::Button::new("预览清理…"),
                )
                .clicked()
            {
                self.pending = Some(Pending::Caches(
                    self.cache_selected
                        .iter()
                        .filter_map(|i| self.caches.get(*i).cloned())
                        .collect(),
                ));
                self.confirmed = false;
            }
            ui.label("清理应用缓存前，请先关闭对应应用。");
        });
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (i, item) in self.caches.iter().enumerate() {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        let mut selected = self.cache_selected.contains(&i);
                        let enabled = item.error.is_none()
                            && !matches!(item.action, CacheAction::Unavailable);
                        if ui
                            .add_enabled(enabled, egui::Checkbox::new(&mut selected, &item.name))
                            .changed()
                        {
                            if selected {
                                self.cache_selected.insert(i);
                            } else {
                                self.cache_selected.remove(&i);
                            }
                        }
                        ui.strong(
                            item.bytes
                                .map(size_text)
                                .unwrap_or_else(|| "释放量待执行后统计".into()),
                        );
                        if matches!(item.action, CacheAction::System(_)) {
                            ui.label("需要系统授权");
                        }
                    });
                    ui.label(item.path.display().to_string());
                    ui.small(&item.description);
                    if let Some(error) = &item.error {
                        ui.colored_label(Color32::from_rgb(210, 135, 90), error);
                    }
                });
            }
        });
    }
    fn rust(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !self.busy() && self.database().exists(),
                    egui::Button::new("识别已扫描的 Rust 项目"),
                )
                .clicked()
            {
                self.discover(true);
            }
            if ui
                .add_enabled(
                    !self.busy() && !self.rust_selected.is_empty(),
                    egui::Button::new("预览清理…"),
                )
                .clicked()
            {
                self.pending = Some(Pending::Rust(
                    self.rust_selected
                        .iter()
                        .filter_map(|i| self.projects.get(*i).cloned())
                        .collect(),
                ));
                self.confirmed = false;
            }
        });
        ui.label("先在“大文件”页扫描项目所在目录。清理整个工作区构建产物，下次构建需要重新编译。");
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (i, project) in self.projects.iter().enumerate() {
                ui.group(|ui| {
                    let mut selected = self.rust_selected.contains(&i);
                    if ui
                        .add_enabled(
                            project.error.is_none(),
                            egui::Checkbox::new(
                                &mut selected,
                                project.manifest.display().to_string(),
                            ),
                        )
                        .changed()
                    {
                        if selected {
                            self.rust_selected.insert(i);
                        } else {
                            self.rust_selected.remove(&i);
                        }
                    }
                    ui.label(format!(
                        "{} · {}",
                        size_text(project.allocated),
                        project.target.display()
                    ));
                    if project.build != project.target && !project.build.as_os_str().is_empty() {
                        ui.small(format!("中间产物：{}", project.build.display()));
                    }
                    if project.newest > 0 {
                        ui.small(format!(
                            "最新产物修改于约 {} 天前（不代表最近使用时间）",
                            (now() - project.newest).max(0) / 86400
                        ));
                    }
                    if project.workspaces.len() > 1 {
                        ui.colored_label(
                            Color32::from_rgb(220, 155, 70),
                            format!("共享目录：关联 {} 个已发现工作区", project.workspaces.len()),
                        );
                    }
                    if let Some(error) = &project.error {
                        ui.small(error);
                    }
                });
            }
        });
    }
    fn settings(&mut self, ui: &mut egui::Ui) {
        ui.label("扫描与清理");
        ui.horizontal(|ui| {
            ui.label("扫描线程数");
            ui.add(egui::DragValue::new(&mut self.settings.threads).range(1..=8));
        });
        ui.horizontal(|ui| {
            ui.label("旧日志/诊断报告保留天数");
            ui.add(egui::DragValue::new(&mut self.settings.retention_days).range(7..=3650));
        });
        ui.horizontal(|ui| {
            ui.label("移动时遇到同名文件");
            ui.selectable_value(&mut self.conflict, ConflictPolicy::Skip, "跳过");
            ui.selectable_value(&mut self.conflict, ConflictPolicy::Number, "自动编号");
        });
        ui.separator();
        ui.label("排除目录（不跟随符号链接，不跨挂载点）");
        let mut remove = None;
        for (i, path) in self.settings.exclusions.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(path.display().to_string());
                if ui.small_button("移除").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.settings.exclusions.remove(i);
        }
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut self.exclusion_input);
            if ui.button("添加排除目录").clicked() && !self.exclusion_input.trim().is_empty()
            {
                self.settings
                    .exclusions
                    .push(self.exclusion_input.trim().into());
                self.exclusion_input.clear();
            }
        });
        if ui.button("保存设置").clicked() {
            self.notice = match self.paths.save(&self.settings) {
                Ok(()) => "设置已保存".into(),
                Err(e) => format!("设置保存失败：{e}"),
            };
        }
        ui.separator();
        ui.label("数据仅保存在本机，不上传文件信息。");
        ui.small(format!("索引：{}", self.paths.cache.display()));
        ui.small("macOS：遇到受隐私保护目录时，在“系统设置 → 隐私与安全性 → 完全磁盘访问权限”中按需授权。系统授权不能绕过 SIP。");
    }
    fn confirmation(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.pending.clone() else {
            return;
        };
        let mut close = false;
        let mut execute = false;
        egui::Window::new("确认操作")
            .collapsible(false)
            .resizable(true)
            .default_width(620.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let (title, paths): (&str, Vec<String>) = match &pending {
                    Pending::Files(plan) => (
                        match plan.action {
                            Action::Trash => "普通文件将移入系统废纸篓，可在系统文件管理器中恢复。",
                            Action::Move { .. } => "移动以下文件；不会覆盖同名文件。",
                        },
                        plan.entries
                            .iter()
                            .map(|e| e.path.display().to_string())
                            .collect(),
                    ),
                    Pending::Caches(items) => (
                        "以下缓存将永久清理，无法从废纸篓恢复。请关闭相关应用。",
                        items
                            .iter()
                            .map(|e| format!("{} · {}", e.name, e.path.display()))
                            .collect(),
                    ),
                    Pending::Rust(items) => (
                        "永久清理整个构建目录，可能影响未扫描到的共享项目；源码保持不变。",
                        items
                            .iter()
                            .map(|e| {
                                format!(
                                    "target: {}\nbuild: {}",
                                    e.target.display(),
                                    e.build.display()
                                )
                            })
                            .collect(),
                    ),
                };
                ui.label(title);
                if let Pending::Files(OperationPlan {
                    action:
                        Action::Move {
                            destination,
                            conflict,
                        },
                    ..
                }) = &pending
                {
                    ui.label(format!(
                        "目标：{} · 冲突处理：{}",
                        destination.display(),
                        if *conflict == ConflictPolicy::Skip {
                            "跳过"
                        } else {
                            "自动编号"
                        }
                    ));
                }
                ui.label(format!("共 {} 项", paths.len()));
                egui::ScrollArea::vertical()
                    .max_height(300.0)
                    .show(ui, |ui| {
                        for path in paths {
                            ui.label(path);
                        }
                    });
                ui.checkbox(&mut self.confirmed, "我已检查上述路径和处理方式");
                ui.horizontal(|ui| {
                    if ui.button("返回").clicked() {
                        close = true;
                    }
                    if ui
                        .add_enabled(
                            self.confirmed && !self.busy(),
                            egui::Button::new("执行操作"),
                        )
                        .clicked()
                    {
                        execute = true;
                    }
                });
            });
        if close || execute {
            self.pending = None;
        }
        if execute {
            self.run_pending(pending);
        }
    }
}
impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        if self.busy() || self.query_running || self.query_dirty {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let frame_start = Instant::now();
        let ctx = ui.ctx().clone();
        if self.busy() || self.query_running || self.query_dirty {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if let Some(start) = self.smoke_start {
            ctx.request_repaint_after(Duration::from_millis(100));
            if start.elapsed() > Duration::from_secs(8)
                && !self.busy()
                && !self.query_running
                && !self.query_dirty
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        egui::Panel::left("navigation")
            .resizable(false)
            .exact_size(180.0)
            .show(ui, |ui| {
                ui.add_space(16.0);
                ui.heading(RichText::new("rsclear").size(28.0));
                ui.small("让磁盘空间一目了然");
                ui.add_space(28.0);
                for (page, name) in [
                    (Page::Files, "大文件"),
                    (Page::Caches, "缓存清理"),
                    (Page::Rust, "Rust 项目"),
                    (Page::History, "任务记录"),
                    (Page::Settings, "设置"),
                ] {
                    ui.selectable_value(&mut self.page, page, name);
                    ui.add_space(6.0);
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.small("v0.1.0 · 本地运行");
                });
            });
        egui::CentralPanel::default().show(ui, |ui| {
            self.header(ui);
            match self.page {
                Page::Files => self.files(ui),
                Page::Caches => self.caches(ui),
                Page::Rust => self.rust(ui),
                Page::Settings => self.settings(ui),
                Page::History => {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for item in self.history.iter().rev() {
                            ui.group(|ui| {
                                ui.strong(format!(
                                    "{} · {}",
                                    match item.status {
                                        ItemStatus::Succeeded => "成功",
                                        ItemStatus::Failed => "失败",
                                        ItemStatus::Skipped => "跳过",
                                        ItemStatus::Cancelled => "取消",
                                    },
                                    item.item
                                ));
                                ui.label(&item.message);
                            });
                        }
                        if self.history.is_empty() {
                            ui.label("还没有执行记录。");
                        }
                    });
                }
            }
        });
        self.confirmation(&ctx);
        if ctx.input(|i| i.viewport().close_requested()) && self.busy() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.notice = "请先取消当前任务，等待操作停止后再关闭窗口。".into();
        }
        if self.smoke_start.is_some() && self.smoke_frames.len() < 10000 {
            self.smoke_frames
                .push(frame_start.elapsed().as_secs_f64() * 1000.0);
        }
    }
    fn on_exit(&mut self) {
        if self.smoke_start.is_some() && !self.smoke_frames.is_empty() {
            self.smoke_frames.sort_by(f64::total_cmp);
            let p95 = self.smoke_frames
                [(self.smoke_frames.len() * 95 / 100).min(self.smoke_frames.len() - 1)];
            eprintln!(
                "RSCLEAR_SMOKE files={} scan_ms={} ui_frames={} ui_cpu_p95_ms={p95:.3}",
                self.progress.files,
                self.progress.elapsed_ms,
                self.smoke_frames.len()
            );
        }
    }
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
