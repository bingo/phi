//! prompt 模板的加载与渲染。
//!
//! 模板是一个 markdown 文件，front matter 里带 `version:`，正文用
//! `## SYSTEM` / `## USER` 分段，占位符是 `{{name}}` 这种形式。
//!
//! 版本号会写进 `analysis.prompt_version`，配合 `input_snapshot` 才能做
//! 「改了 prompt 到底是变好还是变坏」的对比。改了模板内容就换个版本号，
//! 否则历史记录会互相污染。

use anyhow::{bail, Context, Result};
use std::path::Path;

use phi_core::ports::AnalysisInput;

#[derive(Debug, Clone)]
pub struct PromptTemplate {
    pub version: String,
    pub system: String,
    pub user: String,
}

impl PromptTemplate {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| {
            format!(
                "读取 prompt 模板失败: {}\n\
                 （路径是相对于当前工作目录解析的，配置项是 analyzer.prompt_path）",
                path.display()
            )
        })?;
        Self::parse(&raw)
    }

    pub fn parse(raw: &str) -> Result<Self> {
        let (front, body) = split_front_matter(raw);

        let version = front
            .lines()
            .find_map(|l| l.trim().strip_prefix("version:"))
            .map(|v| v.trim().to_string())
            .unwrap_or_else(|| "unversioned".to_string());

        let sys_idx = body.find("## SYSTEM");
        let usr_idx = body.find("## USER");

        let (Some(s), Some(u)) = (sys_idx, usr_idx) else {
            bail!("prompt 模板缺少 `## SYSTEM` 或 `## USER` 段");
        };
        if u < s {
            bail!("prompt 模板里 `## USER` 出现在 `## SYSTEM` 之前");
        }

        let system = body[s + "## SYSTEM".len()..u].trim().to_string();
        let user = body[u + "## USER".len()..].trim().to_string();

        if system.is_empty() || user.is_empty() {
            bail!("prompt 模板的 SYSTEM 或 USER 段是空的");
        }

        Ok(Self {
            version,
            system,
            user,
        })
    }

    /// 用一次分析的输入渲染 user 段。
    pub fn render_user(&self, input: &AnalysisInput) -> String {
        let pairs: [(&str, String); 10] = [
            ("name", input.name.clone()),
            ("tagline", or_dash(&input.tagline)),
            ("description", or_dash(&input.description)),
            ("website", or_dash(&input.website)),
            ("url", input.url.clone()),
            ("topics", or_dash(&input.topics)),
            ("posted_at", or_dash(&input.posted_at)),
            ("votes", input.votes.to_string()),
            ("comments_count", input.comments_count.to_string()),
            ("comments", input.comments.clone()),
        ];

        let mut out = self.user.clone();
        for (k, v) in pairs {
            out = out.replace(&format!("{{{{{k}}}}}"), &v);
        }
        out
    }
}

fn or_dash(s: &str) -> String {
    if s.trim().is_empty() {
        "（无）".to_string()
    } else {
        s.to_string()
    }
}

fn split_front_matter(raw: &str) -> (&str, &str) {
    let trimmed = raw.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let front = &rest[..end];
            let body = &rest[end + 4..];
            return (front, body);
        }
    }
    ("", raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\nversion: v1\n---\n\n## SYSTEM\n\n你是分析师。\n\n## USER\n\n产品：{{name}}，票数 {{votes}}\n\n{{comments}}\n";

    fn input() -> AnalysisInput {
        AnalysisInput {
            name: "Acme".into(),
            tagline: "".into(),
            description: "d".into(),
            website: "w".into(),
            url: "u".into(),
            topics: "t".into(),
            posted_at: "p".into(),
            votes: 42,
            comments_count: 7,
            comments: "- [votes=3] something specific".into(),
        }
    }

    #[test]
    fn parses_version_and_sections() {
        let t = PromptTemplate::parse(SAMPLE).unwrap();
        assert_eq!(t.version, "v1");
        assert_eq!(t.system, "你是分析师。");
        assert!(t.user.contains("{{name}}"));
    }

    #[test]
    fn renders_placeholders() {
        let t = PromptTemplate::parse(SAMPLE).unwrap();
        let out = t.render_user(&input());
        assert!(out.contains("产品：Acme，票数 42"));
        assert!(out.contains("something specific"));
        assert!(!out.contains("{{"), "还有没替换的占位符: {out}");
    }

    #[test]
    fn empty_fields_become_placeholder_text() {
        let t = PromptTemplate::parse("---\nversion: x\n---\n## SYSTEM\ns\n## USER\n[{{tagline}}]")
            .unwrap();
        assert_eq!(t.render_user(&input()), "[（无）]");
    }

    #[test]
    fn rejects_template_without_sections() {
        assert!(PromptTemplate::parse("just some text").is_err());
    }

    #[test]
    fn real_template_file_is_valid() {
        // 仓库里那份模板必须始终可解析 —— 改坏了这里会先炸，而不是等到线上调用。
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../prompts/card.v1.md");
        let t = PromptTemplate::load(&path).expect("prompts/card.v1.md 解析失败");
        assert_eq!(t.version, "v1");
        let rendered = t.render_user(&input());
        assert!(!rendered.contains("{{"), "模板里有未定义的占位符");
    }
}
