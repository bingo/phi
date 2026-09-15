//! OpenRouter 实现。
//!
//! 用裸 `reqwest` 而不是 `async-openai`：我们需要往请求体里塞两个非 OpenAI 标准的
//! 字段（`provider` 和 `usage.include`），而带类型的 SDK 结构体装不下它们。
//! 这里总共就一个 endpoint，手写反而更清楚。
//!
//! # 为什么必须传 provider.require_parameters
//!
//! OpenRouter 的结构化输出支持是 **provider 级**而非 model 级的 —— 同一个 model slug
//! 路由到不同 provider，能力可能不一样。不传这个字段，请求可能被发给一个不支持
//! `json_schema` 的 provider。
//!
//! 好消息是失败模式是显式报错，不会静默降级成一坨散文。所以这个坑会响，不会闷声出错。

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tracing::{debug, info};

use phi_core::config::AnalyzerConfig;
use phi_core::model::{OpportunityCard, Usage};
use phi_core::ports::{AnalysisInput, Analyzer};

use crate::prompt::PromptTemplate;
use crate::schema;

pub struct OpenRouterAnalyzer {
    http: reqwest::Client,
    cfg: AnalyzerConfig,
    api_key: String,
    template: PromptTemplate,
    /// 命令行 `--model` 覆盖配置里的默认值。
    model: String,
}

impl OpenRouterAnalyzer {
    pub fn new(
        cfg: &AnalyzerConfig,
        api_key: String,
        model_override: Option<String>,
    ) -> Result<Self> {
        let template = PromptTemplate::load(&cfg.prompt_path)?;
        Ok(Self {
            http: reqwest::Client::builder()
                // 1M 上下文 + 长输出，超时给宽一点
                .timeout(Duration::from_secs(300))
                .build()?,
            model: model_override.unwrap_or_else(|| cfg.model.clone()),
            cfg: cfg.clone(),
            api_key,
            template,
        })
    }
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<UsageBody>,
    /// OpenRouter 会回填实际路由到的 provider。质量突然波动时第一个查它。
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    message: Option<Message>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Message {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageBody {
    #[serde(default)]
    prompt_tokens: Option<i64>,
    #[serde(default)]
    completion_tokens: Option<i64>,
    #[serde(default)]
    cost: Option<f64>,
}

#[async_trait]
impl Analyzer for OpenRouterAnalyzer {
    fn model(&self) -> &str {
        &self.model
    }

    fn prompt_version(&self) -> &str {
        &self.template.version
    }

    async fn analyze(&self, input: &AnalysisInput) -> Result<(OpportunityCard, Usage)> {
        let user = self.template.render_user(input);

        let mut body = json!({
            "model": self.model,
            "temperature": self.cfg.temperature,
            "messages": [
                { "role": "system", "content": self.template.system },
                { "role": "user",   "content": user },
            ],
            "response_format": schema::response_format(),
            "usage": { "include": true },
        });

        if self.cfg.require_parameters {
            body["provider"] = json!({ "require_parameters": true });
        }

        debug!(model = %self.model, chars = user.len(), "调用 OpenRouter");

        let resp = self
            .http
            .post(format!(
                "{}/chat/completions",
                self.cfg.base_url.trim_end_matches('/')
            ))
            .bearer_auth(&self.api_key)
            .header("X-Title", "phi")
            .json(&body)
            .send()
            .await
            .context("请求 OpenRouter 失败")?;

        let status = resp.status();
        let text = resp.text().await.context("读取 OpenRouter 响应失败")?;

        if !status.is_success() {
            // 最常见的一种：被路由到不支持 json_schema 的 provider。
            // 与其让它静默降级，不如在这里把原因说清楚。
            bail!(
                "OpenRouter 返回 {status}：{text}\n\n\
                 如果报错和 response_format / structured outputs 有关：\n\
                 OpenRouter 的结构化输出支持是 provider 级的，确认 config.toml 里\n\
                 analyzer.require_parameters = true，或者换一个支持该能力的模型。"
            );
        }

        let parsed: ChatResponse = serde_json::from_str(&text)
            .with_context(|| format!("OpenRouter 响应不是预期结构: {text}"))?;

        if let Some(err) = parsed.error {
            bail!("OpenRouter 报错: {err}");
        }

        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("OpenRouter 没有返回任何 choice: {text}"))?;

        if choice.finish_reason.as_deref() == Some("length") {
            bail!("模型输出被长度截断，卡片不完整。调高 max_tokens 或缩短输入。");
        }

        let content = choice
            .message
            .and_then(|m| m.content)
            .ok_or_else(|| anyhow!("OpenRouter 返回的 message 没有 content"))?;

        let card: OpportunityCard = serde_json::from_str(content.trim()).with_context(|| {
            format!(
                "模型输出解析成 OpportunityCard 失败。\n\
                     开了严格模式还出现这种情况，通常说明请求被路由到了不支持 \
                     json_schema 的 provider。\n原始输出：\n{content}"
            )
        })?;

        let usage = Usage {
            tokens_in: parsed.usage.as_ref().and_then(|u| u.prompt_tokens),
            tokens_out: parsed.usage.as_ref().and_then(|u| u.completion_tokens),
            cost_usd: parsed.usage.as_ref().and_then(|u| u.cost),
            provider: parsed.provider,
        };

        info!(
            provider = ?usage.provider,
            tokens_in = ?usage.tokens_in,
            tokens_out = ?usage.tokens_out,
            cost = ?usage.cost_usd,
            "分析完成"
        );

        Ok((card, usage))
    }
}
