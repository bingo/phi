//! 配置加载：`config.toml` + `PHI_` 前缀的环境变量覆盖。

use anyhow::{Context, Result};
use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub db: DbConfig,
    #[serde(default)]
    pub producthunt: ProductHuntConfig,
    #[serde(default)]
    pub analyzer: AnalyzerConfig,
    #[serde(default)]
    pub comment_filter: CommentFilterConfig,
}

/// 存储后端。默认 SQLite —— 单文件、零运维，换 MySQL 只是配置的事。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DbBackend {
    #[default]
    Sqlite,
    Mysql,
}

impl DbBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            DbBackend::Sqlite => "sqlite",
            DbBackend::Mysql => "mysql",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbConfig {
    /// `"sqlite"`（默认）或 `"mysql"`。
    #[serde(default)]
    pub backend: DbBackend,
    /// SQLite 的数据库文件。相对路径按当前工作目录解析。backend = mysql 时忽略。
    pub path: PathBuf,
    /// MySQL 的 DSN：`mysql://user:pass@host:3306/phi`。
    /// 留空则读 `url_env` 指向的环境变量 —— DSN 带密码，优先用环境变量。
    #[serde(default)]
    pub url: String,
    /// 存 MySQL DSN 的环境变量名。只在 `url` 为空时生效。
    #[serde(default = "default_url_env")]
    pub url_env: String,
    pub max_connections: u32,
}

fn default_url_env() -> String {
    "PHI_DB_URL".into()
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            backend: DbBackend::default(),
            path: PathBuf::from("phi.db"),
            url: String::new(),
            url_env: default_url_env(),
            max_connections: 4,
        }
    }
}

impl DbConfig {
    /// MySQL 的 DSN。`url` 优先，否则读 `url_env`。
    ///
    /// 两个都空时给的是**可操作**的错误 —— 配错数据库连接是最容易发生、
    /// 也最容易被一句「connection refused」糊过去的一类失败。
    pub fn resolve_mysql_url(&self) -> Result<String> {
        if !self.url.trim().is_empty() {
            return Ok(self.url.trim().to_string());
        }
        std::env::var(&self.url_env).with_context(|| {
            format!(
                "[db] backend = \"mysql\" 但没有 DSN：config.toml 里填 url，\n\
                 或者 export {}=mysql://user:pass@host:3306/phi",
                self.url_env
            )
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductHuntConfig {
    pub token_env: String,
    pub api_url: String,
    /// 剩余复杂度点数低于这个数就停下来并保存游标。
    /// PH 的配额是 6250 points / 15 分钟，按请求字段计算，嵌套连接会乘起来。
    pub complexity_floor: i64,
}

impl Default for ProductHuntConfig {
    fn default() -> Self {
        Self {
            token_env: "PH_TOKEN".into(),
            api_url: "https://api.producthunt.com/v2/api/graphql".into(),
            complexity_floor: 500,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyzerConfig {
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub temperature: f32,
    /// OpenRouter 的结构化输出支持是 **provider 级**而非 model 级的。
    /// 关掉这个等于允许被路由到不支持 json_schema 的 provider。别关。
    pub require_parameters: bool,
    pub prompt_path: PathBuf,
}

impl Default for AnalyzerConfig {
    fn default() -> Self {
        Self {
            base_url: "https://openrouter.ai/api/v1".into(),
            model: "deepseek/deepseek-v4.1-flash".into(),
            api_key_env: "OPENROUTER_API_KEY".into(),
            temperature: 0.3,
            require_parameters: true,
            prompt_path: PathBuf::from("prompts/card.v1.md"),
        }
    }
}

/// 评论预过滤。目的是**信号密度，不是省钱** —— 模型侧成本可以忽略，
/// 但 200 条 "Congrats 🎉" 会稀释模型对真正抱怨那几条的注意力。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommentFilterConfig {
    /// 按**字符**数算，不是字节。这个默认值是照英文语料调的 —— 中文单字信息量
    /// 高得多，60 个中文字符已经是一条有内容的抱怨。接中文源时要调低。
    pub min_length: usize,
    /// 0 表示不限。
    pub top_n: usize,
    /// maker 回复里常有路线图和定价解释，信息价值高，无条件保留。
    pub keep_all_maker_comments: bool,
    pub noise_max_length: usize,
    pub noise_patterns: Vec<String>,
}

impl Default for CommentFilterConfig {
    fn default() -> Self {
        Self {
            min_length: 60,
            top_n: 80,
            keep_all_maker_comments: true,
            noise_max_length: 150,
            noise_patterns: vec![
                r"(?i)congrat".into(),
                r"(?i)good luck".into(),
                r"(?i)all the best".into(),
                r"(?i)looks (great|awesome|cool|amazing)".into(),
                r"(?i)love (this|it)".into(),
                r"(?i)^\s*(nice|cool|awesome|great|amazing|wow)\b".into(),
            ],
        }
    }
}

impl Config {
    /// 从 `path`（缺失则用默认值）加载，再让 `PHI_` 环境变量覆盖。
    /// 层级用双下划线：`PHI_ANALYZER__MODEL=...`
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut fig = Figment::from(figment::providers::Serialized::defaults(Config::default()));
        if let Some(p) = path {
            if p.exists() {
                fig = fig.merge(Toml::file(p));
            }
        } else if Path::new("config.toml").exists() {
            fig = fig.merge(Toml::file("config.toml"));
        }
        fig.merge(Env::prefixed("PHI_").split("__"))
            .extract()
            .context("解析配置失败")
    }

    /// 读取某个配置项指定的环境变量，缺失时给出可操作的错误信息。
    pub fn secret(var: &str) -> Result<String> {
        std::env::var(var).with_context(|| format!("环境变量 {var} 未设置"))
    }
}
