//! TUI 的纯状态机：按键 → 状态变化 + 一个需要外部执行的 [`Effect`]。
//!
//! 这里不碰终端、不碰数据库，所以可以直接构造 `App` 喂按键来测。

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;
use ratatui_textarea::TextArea;
use std::time::Instant;

use phi_core::usecase::AnalyzeStage;

use phi_core::model::{
    AnalysisState, ItemDetail, ItemSummary, Overview, OverviewQuery, SortKey, Tri, Verdict,
};

/// 阈值每次 +/- 的步长
const THRESHOLD_STEP: i64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Card,
    Notes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// 正在输入搜索词。输入是实时生效的
    Search,
    NoteEdit,
    Help,
    /// 按了 `a`，等 y / Enter 确认。分析要花模型费用和 PH 配额，不能手滑就发出去
    ConfirmAnalyze,
}

/// 状态机要求外部执行的 IO。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    Quit,
    /// 按当前 `query` 重新拉列表
    Reload,
    /// 选中项变了，拉它的详情
    LoadDetail(i64),
    SaveNote {
        item_id: i64,
        body: String,
    },
    ExternalEditor {
        item_id: i64,
    },
    OpenUrl(String),
    /// 后台开始分析这条 item
    StartAnalyze {
        item_id: i64,
    },
    /// 按顺序执行多个 effect
    Batch(Vec<Effect>),
}

/// 一个正在后台跑的分析
#[derive(Debug, Clone)]
pub struct Job {
    pub item_id: i64,
    pub name: String,
    pub stage: Option<AnalyzeStage>,
    pub started: Instant,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub text: String,
    pub is_error: bool,
}

pub struct App {
    pub query: OverviewQuery,
    /// 启动时 `--min-comments` 给的阈值，`x` 清空筛选时回到它
    initial_min_signal: i64,
    pub rows: Vec<ItemSummary>,
    pub total: usize,
    pub table: TableState,
    pub detail: Option<ItemDetail>,
    /// 当前看的是第几个分析版本，0 = 最新
    pub version: usize,
    pub card_scroll: u16,
    pub notes_scroll: u16,
    /// 由 ui 在绘制时回填，用于翻页步长
    pub card_page: u16,
    pub notes_page: u16,
    pub focus: Focus,
    pub mode: Mode,
    pub search_input: String,
    /// 进入搜索前的搜索词，Esc 时恢复
    search_before: Option<String>,
    pub editor: TextArea<'static>,
    /// 编辑器里有内容时，第一次 Esc 只武装、第二次才真正丢弃
    discard_armed: bool,
    pub status: Option<Status>,
    pub jobs: Vec<Job>,
    /// 等待确认分析的 item
    pending_analyze: Option<(i64, String)>,
    /// 还有分析在跑时，第一次 q 只武装
    quit_armed: bool,
}

impl App {
    pub fn new(min_signal: i64) -> Self {
        Self {
            query: OverviewQuery {
                min_signal,
                ..Default::default()
            },
            initial_min_signal: min_signal,
            rows: Vec::new(),
            total: 0,
            table: TableState::default(),
            detail: None,
            version: 0,
            card_scroll: 0,
            notes_scroll: 0,
            card_page: 10,
            notes_page: 10,
            focus: Focus::List,
            mode: Mode::Normal,
            search_input: String::new(),
            search_before: None,
            editor: new_editor(),
            discard_armed: false,
            status: None,
            jobs: Vec::new(),
            pending_analyze: None,
            quit_armed: false,
        }
    }

    pub fn job_for(&self, item_id: i64) -> Option<&Job> {
        self.jobs.iter().find(|j| j.item_id == item_id)
    }

    pub fn job_started(&mut self, item_id: i64, name: String) {
        self.jobs.push(Job {
            item_id,
            name: name.clone(),
            stage: None,
            started: Instant::now(),
        });
        self.info(format!("已在后台开始分析「{name}」，可以继续浏览"));
    }

    pub fn job_stage(&mut self, item_id: i64, stage: AnalyzeStage) {
        if let Some(j) = self.jobs.iter_mut().find(|j| j.item_id == item_id) {
            j.stage = Some(stage);
        }
    }

    /// 分析结束（成功是新 analysis 的 id，失败是错误信息）。调用方随后应当重新加载列表和详情。
    pub fn job_finished(&mut self, item_id: i64, result: Result<i64, String>) {
        let Some(pos) = self.jobs.iter().position(|j| j.item_id == item_id) else {
            return;
        };
        let job = self.jobs.remove(pos);
        let secs = job.started.elapsed().as_secs();
        match result {
            Ok(analysis_id) => {
                // 新版本插在最前面：正在看这条的话，切回最新版本
                if self.detail.as_ref().map(|d| d.item.id) == Some(item_id) {
                    self.version = 0;
                    self.card_scroll = 0;
                }
                self.info(format!(
                    "「{}」分析完成（analysis #{analysis_id}，{secs}s）",
                    job.name
                ));
            }
            Err(e) => self.error(format!("「{}」分析失败：{e}", job.name)),
        }
        if self.jobs.is_empty() {
            self.quit_armed = false;
        }
    }

    pub fn selected_id(&self) -> Option<i64> {
        self.table
            .selected()
            .and_then(|i| self.rows.get(i))
            .map(|r| r.item.id)
    }

    /// 换上新列表，尽量保持选中同一条 item。返回选中的 item 是否变了。
    pub fn set_rows(&mut self, overview: Overview) -> bool {
        let before = self.selected_id();
        self.rows = overview.rows;
        self.total = overview.total;

        let idx = if self.rows.is_empty() {
            None
        } else {
            before
                .and_then(|id| self.rows.iter().position(|r| r.item.id == id))
                // 原来那条被筛掉了：停在原来的位置附近，而不是跳回顶部
                .or(Some(
                    self.table.selected().unwrap_or(0).min(self.rows.len() - 1),
                ))
        };
        self.table.select(idx);

        let after = self.selected_id();
        if after.is_none() {
            self.detail = None;
        }
        after != before
    }

    pub fn set_detail(&mut self, detail: ItemDetail) {
        let same_item = self.detail.as_ref().map(|d| d.item.id) == Some(detail.item.id);
        if !same_item {
            self.version = 0;
            self.card_scroll = 0;
            self.notes_scroll = 0;
        }
        self.version = self.version.min(detail.analyses.len().saturating_sub(1));
        self.detail = Some(detail);
    }

    pub fn info(&mut self, text: impl Into<String>) {
        self.status = Some(Status {
            text: text.into(),
            is_error: false,
        });
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.status = Some(Status {
            text: text.into(),
            is_error: true,
        });
    }

    /// 笔记保存成功后调用
    pub fn note_saved(&mut self) {
        self.editor = new_editor();
        self.mode = Mode::Normal;
        self.discard_armed = false;
        self.notes_scroll = 0;
    }

    pub fn handle_event(&mut self, ev: Event) -> Effect {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.handle_key(k),
            Event::Paste(s) => match self.mode {
                Mode::NoteEdit => {
                    self.editor.insert_str(s);
                    Effect::None
                }
                Mode::Search => {
                    self.search_input.push_str(&s.replace(['\n', '\r'], " "));
                    self.apply_search_input()
                }
                _ => Effect::None,
            },
            _ => Effect::None,
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> Effect {
        // 终端把「Esc 紧跟一个字符」编码成 ESC 前缀，和 Alt+字符是同一个字节序列，读出来分不清。
        // 按用户最可能的本意拆成两次按键：先 Esc，再那个字符。编辑器里除外 —— 那里 Alt 组合键有自己的含义
        if let KeyCode::Char(_) = k.code {
            if k.modifiers.contains(KeyModifiers::ALT) && self.mode != Mode::NoteEdit {
                let first = self.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                let second =
                    self.handle_key(KeyEvent::new(k.code, k.modifiers - KeyModifiers::ALT));
                return batch(first, second);
            }
        }
        match self.mode {
            Mode::Help => {
                self.mode = Mode::Normal;
                Effect::None
            }
            Mode::Search => self.key_search(k),
            Mode::NoteEdit => self.key_note(k),
            Mode::ConfirmAnalyze => self.key_confirm_analyze(k),
            Mode::Normal => {
                self.status = None;
                self.key_normal(k)
            }
        }
    }

    fn key_normal(&mut self, k: KeyEvent) -> Effect {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let is_quit =
            matches!(k.code, KeyCode::Char('q')) || matches!(k.code, KeyCode::Char('c') if ctrl);
        if !is_quit {
            self.quit_armed = false;
        }
        match k.code {
            _ if is_quit => {
                if self.jobs.is_empty() || self.quit_armed {
                    Effect::Quit
                } else {
                    self.quit_armed = true;
                    self.error(format!(
                        "还有 {} 个分析在跑，退出会中断它们（已花的模型费用不会退）。再按一次 q 退出",
                        self.jobs.len()
                    ));
                    Effect::None
                }
            }
            KeyCode::Char('?') => {
                self.mode = Mode::Help;
                Effect::None
            }

            // ---- 焦点
            KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right => {
                self.focus = match self.focus {
                    Focus::List => Focus::Card,
                    Focus::Card => Focus::Notes,
                    Focus::Notes => Focus::List,
                };
                Effect::None
            }
            KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left => {
                self.focus = match self.focus {
                    Focus::List => Focus::Notes,
                    Focus::Card => Focus::List,
                    Focus::Notes => Focus::Card,
                };
                Effect::None
            }

            // ---- 移动 / 滚动（作用于当前焦点栏）
            KeyCode::Char('j') | KeyCode::Down => self.step(1),
            KeyCode::Char('k') | KeyCode::Up => self.step(-1),
            KeyCode::Char('d') if ctrl => self.page(1),
            KeyCode::Char('u') if ctrl => self.page(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.page(1),
            KeyCode::PageUp => self.page(-1),
            KeyCode::Char('g') | KeyCode::Home => self.step(i32::MIN / 2),
            KeyCode::Char('G') | KeyCode::End => self.step(i32::MAX / 2),

            // ---- 筛选 / 排序
            KeyCode::Char('/') => {
                self.search_before = self.query.text.clone();
                self.search_input = self.query.text.clone().unwrap_or_default();
                self.mode = Mode::Search;
                Effect::None
            }
            KeyCode::Esc if self.query.text.is_some() => {
                self.query.text = None;
                Effect::Reload
            }
            KeyCode::Char('s') => {
                self.query.sort = match self.query.sort {
                    SortKey::Comments => SortKey::Votes,
                    SortKey::Votes => SortKey::Newest,
                    SortKey::Newest => SortKey::Name,
                    SortKey::Name => SortKey::Comments,
                };
                Effect::Reload
            }
            KeyCode::Char('f') => {
                self.query.state = match self.query.state {
                    AnalysisState::All => AnalysisState::Analyzed,
                    AnalysisState::Analyzed => AnalysisState::Pending,
                    AnalysisState::Pending => AnalysisState::All,
                };
                Effect::Reload
            }
            KeyCode::Char('v') => {
                self.query.verdict = match self.query.verdict {
                    None => Some(Verdict::Follow),
                    Some(Verdict::Follow) => Some(Verdict::Watch),
                    Some(Verdict::Watch) => Some(Verdict::Drop),
                    Some(Verdict::Drop) => None,
                };
                Effect::Reload
            }
            KeyCode::Char('b') => cycle_tri(&mut self.query.buildable),
            KeyCode::Char('w') => cycle_tri(&mut self.query.worth_it),
            KeyCode::Char('r') => cycle_tri(&mut self.query.reachable),
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.query.min_signal += THRESHOLD_STEP;
                Effect::Reload
            }
            KeyCode::Char('-') => {
                self.query.min_signal = (self.query.min_signal - THRESHOLD_STEP).max(0);
                Effect::Reload
            }
            KeyCode::Char('x') => {
                self.query = OverviewQuery {
                    min_signal: self.initial_min_signal,
                    sort: self.query.sort,
                    ..Default::default()
                };
                self.info("已清空筛选");
                Effect::Reload
            }
            KeyCode::Char('R') => {
                self.info("已刷新");
                Effect::Reload
            }

            // ---- 分析版本：analysis 表只追加，旧版本一直在
            KeyCode::Char('[') => self.switch_version(1),
            KeyCode::Char(']') => self.switch_version(-1),

            // ---- 笔记
            KeyCode::Char('n') => match self.selected_id() {
                Some(_) => {
                    self.mode = Mode::NoteEdit;
                    self.focus = Focus::Notes;
                    self.discard_armed = false;
                    Effect::None
                }
                None => self.nothing_selected(),
            },
            KeyCode::Char('E') => match self.selected_id() {
                Some(item_id) => Effect::ExternalEditor { item_id },
                None => self.nothing_selected(),
            },

            KeyCode::Char('o') => match self.detail.as_ref() {
                Some(d) => Effect::OpenUrl(d.item.url.clone()),
                None => self.nothing_selected(),
            },

            KeyCode::Char('a') => self.ask_analyze(),

            _ => Effect::None,
        }
    }

    fn ask_analyze(&mut self) -> Effect {
        let Some(row) = self.table.selected().and_then(|i| self.rows.get(i)) else {
            return self.nothing_selected();
        };
        let (item_id, name) = (row.item.id, row.item.name.clone());
        if self.job_for(item_id).is_some() {
            self.error(format!("「{name}」已经在分析了"));
            return Effect::None;
        }
        let mut what = vec!["调用模型（一次约 $0.01）".to_string()];
        if row.item.comments_fetched_at.is_none() {
            what.insert(0, "先抓评论（消耗 PH 配额）".into());
        }
        if row.analysis_count > 0 {
            what.push(format!(
                "在已有 {} 个版本之后追加一个新版本",
                row.analysis_count
            ));
        }
        self.info(format!(
            "分析「{name}」？会{}。y / Enter 确认，其他键取消",
            what.join("，")
        ));
        self.pending_analyze = Some((item_id, name));
        self.mode = Mode::ConfirmAnalyze;
        Effect::None
    }

    fn key_confirm_analyze(&mut self, k: KeyEvent) -> Effect {
        self.mode = Mode::Normal;
        let pending = self.pending_analyze.take();
        match (k.code, pending) {
            (KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter, Some((item_id, _))) => {
                self.status = None;
                Effect::StartAnalyze { item_id }
            }
            _ => {
                self.info("已取消分析");
                Effect::None
            }
        }
    }

    fn key_search(&mut self, k: KeyEvent) -> Effect {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                Effect::None
            }
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.query.text = self.search_before.take();
                Effect::Reload
            }
            KeyCode::Char('u') if ctrl => {
                self.search_input.clear();
                self.apply_search_input()
            }
            KeyCode::Char('c') if ctrl => {
                self.mode = Mode::Normal;
                self.query.text = self.search_before.take();
                Effect::Reload
            }
            KeyCode::Backspace => {
                self.search_input.pop();
                self.apply_search_input()
            }
            KeyCode::Char(c) if !ctrl => {
                self.search_input.push(c);
                self.apply_search_input()
            }
            _ => Effect::None,
        }
    }

    fn apply_search_input(&mut self) -> Effect {
        let t = self.search_input.trim();
        self.query.text = (!t.is_empty()).then(|| t.to_string());
        Effect::Reload
    }

    fn key_note(&mut self, k: KeyEvent) -> Effect {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        match k.code {
            KeyCode::Char('s') if ctrl => self.submit_note(),
            KeyCode::Enter if alt => self.submit_note(),
            KeyCode::Esc => self.cancel_note(),
            KeyCode::Char('c') if ctrl => self.cancel_note(),
            _ => {
                self.discard_armed = false;
                self.status = None;
                self.editor.input(k);
                Effect::None
            }
        }
    }

    pub fn editor_text(&self) -> String {
        self.editor
            .clone()
            .into_lines()
            .join("\n")
            .trim()
            .to_string()
    }

    fn submit_note(&mut self) -> Effect {
        let body = self.editor_text();
        match self.selected_id() {
            _ if body.is_empty() => {
                self.error("笔记是空的，没有保存");
                Effect::None
            }
            Some(item_id) => Effect::SaveNote { item_id, body },
            None => self.nothing_selected(),
        }
    }

    fn cancel_note(&mut self) -> Effect {
        if self.editor_text().is_empty() || self.discard_armed {
            self.editor = new_editor();
            self.mode = Mode::Normal;
            self.discard_armed = false;
            self.status = None;
        } else {
            self.discard_armed = true;
            self.error("再按一次 Esc 放弃这条笔记（Ctrl+S 保存）");
        }
        Effect::None
    }

    fn nothing_selected(&mut self) -> Effect {
        self.error("没有选中的条目");
        Effect::None
    }

    fn step(&mut self, delta: i32) -> Effect {
        match self.focus {
            Focus::List => self.move_selection(delta),
            Focus::Card => {
                self.card_scroll = add_clamped(self.card_scroll, delta);
                Effect::None
            }
            Focus::Notes => {
                self.notes_scroll = add_clamped(self.notes_scroll, delta);
                Effect::None
            }
        }
    }

    fn page(&mut self, dir: i32) -> Effect {
        let page = match self.focus {
            Focus::List => 10,
            Focus::Card => self.card_page.saturating_sub(2).max(1) as i32,
            Focus::Notes => self.notes_page.saturating_sub(2).max(1) as i32,
        };
        self.step(dir * page)
    }

    fn move_selection(&mut self, delta: i32) -> Effect {
        if self.rows.is_empty() {
            return Effect::None;
        }
        let last = self.rows.len() as i64 - 1;
        let cur = self.table.selected().unwrap_or(0) as i64;
        let next = (cur + delta as i64).clamp(0, last) as usize;
        if Some(next) == self.table.selected() {
            return Effect::None;
        }
        self.table.select(Some(next));
        match self.selected_id() {
            Some(id) => Effect::LoadDetail(id),
            None => Effect::None,
        }
    }

    /// `delta > 0` 往旧版本走
    fn switch_version(&mut self, delta: i32) -> Effect {
        let n = self.detail.as_ref().map_or(0, |d| d.analyses.len());
        if n <= 1 {
            self.error("这条只有一个分析版本");
            return Effect::None;
        }
        let next = (self.version as i64 + delta as i64).clamp(0, n as i64 - 1) as usize;
        if next != self.version {
            self.version = next;
            self.card_scroll = 0;
        }
        Effect::None
    }
}

/// 合并两次按键的 effect，去掉空的
fn batch(first: Effect, second: Effect) -> Effect {
    match (first, second) {
        (Effect::None, e) | (e, Effect::None) => e,
        (a, b) => Effect::Batch(vec![a, b]),
    }
}

fn cycle_tri(slot: &mut Option<Tri>) -> Effect {
    *slot = match slot {
        None => Some(Tri::Yes),
        Some(Tri::Yes) => Some(Tri::Unsure),
        Some(Tri::Unsure) => Some(Tri::No),
        Some(Tri::No) => None,
    };
    Effect::Reload
}

fn add_clamped(v: u16, delta: i32) -> u16 {
    (v as i32 + delta).clamp(0, u16::MAX as i32) as u16
}

fn new_editor() -> TextArea<'static> {
    let mut t = TextArea::default();
    t.set_placeholder_text("写下你自己的判断。这条线和 AI 分析完全独立，重跑分析不会动它。");
    t
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use phi_core::model::ItemDetail;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn code(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn app_with_rows() -> App {
        let mut app = App::new(0);
        app.set_rows(overview(vec![
            summary(1, "Caddi", 41, Some(Verdict::Watch)),
            summary(2, "Zeta", 20, None),
            summary(3, "Acme", 5, Some(Verdict::Follow)),
        ]));
        app
    }

    fn detail(item_id: i64, versions: usize) -> ItemDetail {
        ItemDetail {
            item: item(item_id, "Caddi", 41),
            analyses: (0..versions)
                .map(|v| analysis(10 + v as i64, item_id, Verdict::Watch))
                .collect(),
            notes: vec![],
        }
    }

    #[test]
    fn list_navigation_loads_detail_and_clamps() {
        let mut app = app_with_rows();
        assert_eq!(app.selected_id(), Some(1));
        assert_eq!(
            app.handle_key(key('k')),
            Effect::None,
            "顶部再往上不该触发加载"
        );
        assert_eq!(app.handle_key(key('j')), Effect::LoadDetail(2));
        assert_eq!(app.handle_key(key('G')), Effect::LoadDetail(3));
        assert_eq!(app.handle_key(key('j')), Effect::None);
        assert_eq!(app.handle_key(key('g')), Effect::LoadDetail(1));
    }

    #[test]
    fn j_k_scroll_the_focused_pane_instead_of_the_list() {
        let mut app = app_with_rows();
        app.handle_key(code(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Card);
        assert_eq!(app.handle_key(key('j')), Effect::None);
        assert_eq!(app.card_scroll, 1);
        assert_eq!(app.selected_id(), Some(1), "焦点在卡片栏时列表选中不该动");
    }

    #[test]
    fn set_rows_keeps_selection_by_id() {
        let mut app = app_with_rows();
        app.handle_key(key('j')); // 选中 Zeta（id 2）
                                  // 换了排序：Zeta 跑到第一位
        let changed = app.set_rows(overview(vec![
            summary(2, "Zeta", 20, None),
            summary(1, "Caddi", 41, Some(Verdict::Watch)),
        ]));
        assert!(!changed);
        assert_eq!(app.selected_id(), Some(2));

        // Zeta 被筛掉了：停在原位置附近，而不是回到顶部
        let changed = app.set_rows(overview(vec![summary(
            1,
            "Caddi",
            41,
            Some(Verdict::Watch),
        )]));
        assert!(changed);
        assert_eq!(app.selected_id(), Some(1));

        // 全部被筛掉
        assert!(app.set_rows(overview(vec![])));
        assert_eq!(app.selected_id(), None);
        assert!(app.detail.is_none());
    }

    #[test]
    fn filter_keys_update_query_and_reload() {
        let mut app = App::new(15);
        assert_eq!(app.handle_key(key('v')), Effect::Reload);
        assert_eq!(app.query.verdict, Some(Verdict::Follow));
        app.handle_key(key('b'));
        app.handle_key(key('b'));
        assert_eq!(app.query.buildable, Some(Tri::Unsure));
        app.handle_key(key('r'));
        assert_eq!(app.query.reachable, Some(Tri::Yes));
        app.handle_key(key('f'));
        assert_eq!(app.query.state, AnalysisState::Analyzed);
        app.handle_key(key('s'));
        assert_eq!(app.query.sort, SortKey::Votes);

        for _ in 0..5 {
            app.handle_key(key('-'));
        }
        assert_eq!(app.query.min_signal, 0, "阈值不能减成负数");
        app.handle_key(key('+'));
        assert_eq!(app.query.min_signal, 5);

        // x：筛选全清，阈值回到启动值，排序保留
        assert_eq!(app.handle_key(key('x')), Effect::Reload);
        assert_eq!(app.query.verdict, None);
        assert_eq!(app.query.buildable, None);
        assert_eq!(app.query.state, AnalysisState::All);
        assert_eq!(app.query.min_signal, 15);
        assert_eq!(app.query.sort, SortKey::Votes);
    }

    #[test]
    fn search_is_live_and_esc_restores_previous() {
        let mut app = app_with_rows();
        app.handle_key(key('/'));
        assert_eq!(app.mode, Mode::Search);
        assert_eq!(app.handle_key(key('律')), Effect::Reload);
        app.handle_key(key('所'));
        assert_eq!(app.query.text.as_deref(), Some("律所"));
        app.handle_key(code(KeyCode::Enter));
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.query.text.as_deref(), Some("律所"));

        // 再进搜索改了一半按 Esc：回到「律所」
        app.handle_key(key('/'));
        app.handle_key(code(KeyCode::Backspace));
        assert_eq!(app.query.text.as_deref(), Some("律"));
        assert_eq!(app.handle_key(code(KeyCode::Esc)), Effect::Reload);
        assert_eq!(app.query.text.as_deref(), Some("律所"));

        // 普通模式下 Esc 清除搜索
        assert_eq!(app.handle_key(code(KeyCode::Esc)), Effect::Reload);
        assert_eq!(app.query.text, None);
        // 单独的 q 在搜索框里是输入，不是退出
        app.handle_key(key('/'));
        assert_eq!(app.handle_key(key('q')), Effect::Reload);
        assert_eq!(app.query.text.as_deref(), Some("q"));
    }

    #[test]
    fn note_editing_saves_and_guards_against_accidental_discard() {
        let mut app = app_with_rows();
        app.handle_key(key('n'));
        assert_eq!(app.mode, Mode::NoteEdit);
        assert_eq!(app.focus, Focus::Notes);

        // 空笔记不保存
        assert_eq!(app.handle_key(ctrl('s')), Effect::None);
        assert!(app.status.as_ref().unwrap().is_error);

        for c in "获客是真问题".chars() {
            app.handle_key(key(c));
        }
        // q 在编辑器里是输入
        assert_eq!(app.handle_key(key('q')), Effect::None);
        assert_eq!(app.mode, Mode::NoteEdit);

        // 有内容时第一次 Esc 只提醒
        app.handle_key(code(KeyCode::Esc));
        assert_eq!(app.mode, Mode::NoteEdit);
        // 继续打字会解除武装
        app.handle_key(key('!'));
        app.handle_key(code(KeyCode::Esc));
        assert_eq!(app.mode, Mode::NoteEdit, "打字之后需要重新按两次 Esc");

        assert_eq!(
            app.handle_key(ctrl('s')),
            Effect::SaveNote {
                item_id: 1,
                body: "获客是真问题q!".into()
            }
        );
        // 保存由外部执行；成功前编辑器内容保留
        assert_eq!(app.editor_text(), "获客是真问题q!");
        app.note_saved();
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.editor_text(), "");

        // 两次 Esc 放弃
        app.handle_key(key('n'));
        app.handle_key(key('x'));
        app.handle_key(code(KeyCode::Esc));
        app.handle_key(code(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.editor_text(), "");
    }

    #[test]
    fn version_switching_clamps() {
        let mut app = app_with_rows();
        app.set_detail(detail(1, 1));
        app.handle_key(key('['));
        assert!(
            app.status.as_ref().unwrap().is_error,
            "只有一个版本时应当提示"
        );

        app.set_detail(detail(1, 3));
        app.handle_key(key('['));
        app.handle_key(key('['));
        app.handle_key(key('['));
        assert_eq!(app.version, 2);
        app.handle_key(key(']'));
        assert_eq!(app.version, 1);

        // 同一条 item 刷新详情时保留版本位置；换 item 时回到最新
        app.set_detail(detail(1, 3));
        assert_eq!(app.version, 1);
        app.set_detail(detail(2, 3));
        assert_eq!(app.version, 0);
    }

    #[test]
    fn nothing_selected_is_an_error_not_a_crash() {
        let mut app = App::new(0);
        for c in ['n', 'E', 'o'] {
            assert_eq!(app.handle_key(key(c)), Effect::None);
            assert!(
                app.status.as_ref().unwrap().is_error,
                "按 {c} 时应当提示没有选中"
            );
        }
        assert_eq!(app.mode, Mode::Normal);
        // 空列表上移动是静默的空操作
        assert_eq!(app.handle_key(key('j')), Effect::None);
    }
}

#[cfg(test)]
mod analyze_and_alt_tests {
    use super::super::fixtures::*;
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    fn app() -> App {
        let mut app = App::new(0);
        let mut analyzed = summary(2, "Zeta", 20, Some(Verdict::Watch));
        analyzed.item.comments_fetched_at = Some("2026-09-15T00:00:00Z".into());
        analyzed.analysis_count = 2;
        app.set_rows(overview(vec![summary(1, "Caddi", 41, None), analyzed]));
        app
    }

    #[test]
    fn analyze_requires_confirmation() {
        let mut app = app();
        assert_eq!(app.handle_key(key('a')), Effect::None, "按 a 不能直接开跑");
        assert_eq!(app.mode, Mode::ConfirmAnalyze);
        let prompt = &app.status.as_ref().unwrap().text;
        assert!(
            prompt.contains("Caddi") && prompt.contains("先抓评论"),
            "{prompt}"
        );

        // 其他键取消，而且这个键不会被当成普通命令执行
        assert_eq!(app.handle_key(key('q')), Effect::None);
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.status.as_ref().unwrap().text.contains("取消"));

        // y 确认
        app.handle_key(key('a'));
        assert_eq!(
            app.handle_key(key('y')),
            Effect::StartAnalyze { item_id: 1 }
        );
        assert_eq!(app.mode, Mode::Normal);

        // Enter 也能确认；已分析过、评论已抓的条目，提示里说追加新版本、不提抓评论
        app.handle_key(key('j'));
        app.handle_key(key('a'));
        let prompt = app.status.as_ref().unwrap().text.clone();
        assert!(
            prompt.contains("已有 2 个版本") && !prompt.contains("抓评论"),
            "{prompt}"
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Effect::StartAnalyze { item_id: 2 }
        );
    }

    #[test]
    fn job_lifecycle_and_duplicate_guard() {
        let mut app = app();
        app.job_started(1, "Caddi".into());
        assert!(app.job_for(1).is_some());
        assert_eq!(app.job_for(1).unwrap().stage, None);

        // 同一条不能重复发起
        assert_eq!(app.handle_key(key('a')), Effect::None);
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.status.as_ref().unwrap().is_error);

        app.job_stage(1, AnalyzeStage::FetchingComments);
        assert_eq!(
            app.job_for(1).unwrap().stage,
            Some(AnalyzeStage::FetchingComments)
        );

        // 正在看旧版本时分析完成：切回最新版本
        app.detail = Some(phi_core::model::ItemDetail {
            item: item(1, "Caddi", 41),
            analyses: vec![
                analysis(11, 1, Verdict::Watch),
                analysis(10, 1, Verdict::Drop),
            ],
            notes: vec![],
        });
        app.version = 1;
        app.card_scroll = 30;
        app.job_finished(1, Ok(12));
        assert!(app.jobs.is_empty());
        assert_eq!((app.version, app.card_scroll), (0, 0));
        assert!(app.status.as_ref().unwrap().text.contains("#12"));

        // 失败：报错并移除任务
        app.job_started(2, "Zeta".into());
        app.job_finished(2, Err("模型超时".into()));
        assert!(app.jobs.is_empty());
        let s = app.status.as_ref().unwrap();
        assert!(s.is_error && s.text.contains("模型超时"));

        // 不认识的 id 不 panic
        app.job_finished(99, Ok(1));
    }

    #[test]
    fn quitting_with_running_jobs_needs_a_second_press() {
        let mut app = app();
        app.job_started(1, "Caddi".into());
        assert_eq!(app.handle_key(key('q')), Effect::None);
        assert!(app.status.as_ref().unwrap().text.contains("再按一次 q"));
        // 中间按了别的键，要重新按两次
        app.handle_key(key('j'));
        assert_eq!(app.handle_key(key('q')), Effect::None);
        assert_eq!(app.handle_key(key('q')), Effect::Quit);

        // 任务跑完后一次就退
        let mut app = self::app();
        app.job_started(1, "Caddi".into());
        app.handle_key(key('q'));
        app.job_finished(1, Ok(1));
        let mut fresh = self::app();
        assert_eq!(fresh.handle_key(key('q')), Effect::Quit);
        assert_eq!(app.handle_key(key('q')), Effect::Quit);
    }

    #[test]
    fn esc_prefixed_char_is_split_into_esc_then_char() {
        // tmux 里快速连发 `Escape /` 会被读成 Alt+/
        let mut app = app();
        app.query.text = Some("律所".into());
        let effect = app.handle_key(alt('/'));
        assert_eq!(
            effect,
            Effect::Reload,
            "Esc 清掉搜索（Reload），/ 进入搜索（无 effect）"
        );
        assert_eq!(app.query.text, None);
        assert_eq!(app.mode, Mode::Search);
        assert_eq!(app.search_input, "", "不能把旧搜索词带进新的搜索框");

        // 搜索框里 Alt+x：Esc 退出搜索并恢复旧词，x 清空筛选 —— 两个 Reload 都要执行
        let mut app = self::app();
        app.handle_key(key('/'));
        app.handle_key(key('z'));
        assert_eq!(
            app.handle_key(alt('x')),
            Effect::Batch(vec![Effect::Reload, Effect::Reload])
        );
        assert_eq!(app.mode, Mode::Normal);

        // 没有搜索时 Alt+j = 移动选中
        let mut app = self::app();
        assert_eq!(app.handle_key(alt('j')), Effect::LoadDetail(2));

        // 确认框里 Alt+y：Esc 先取消，y 不应再触发分析
        let mut app = self::app();
        app.handle_key(key('a'));
        assert_eq!(app.handle_key(alt('y')), Effect::None);
        assert_eq!(app.mode, Mode::Normal);

        // 编辑器里 Alt 组合键原样交给编辑器（Alt+b 是按词后退），不拆
        let mut app = self::app();
        app.handle_key(key('n'));
        for c in "hello world".chars() {
            app.handle_key(key(c));
        }
        assert_eq!(app.handle_key(alt('b')), Effect::None);
        assert_eq!(app.mode, Mode::NoteEdit);
        assert_eq!(app.editor_text(), "hello world");
    }
}
