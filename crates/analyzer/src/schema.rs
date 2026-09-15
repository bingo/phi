//! 从 `OpportunityCard` 生成严格模式的 JSON Schema。
//!
//! 用 `schemars` 从 Rust 类型生成，而不是手写 schema —— 这样 struct 和 schema
//! 永远一致，改字段时不会出现「schema 改了但反序列化目标忘了改」这种只在运行期
//! 才炸的问题。struct 上的文档注释会变成 schema 里的 `description`，
//! 所以像「不要翻译」这类约束写在类型定义上比写在 system prompt 里更不容易被忽略。
//!
//! 严格模式（OpenAI 兼容）要求：
//! - 每个 object 的 `required` 必须列出**全部** properties
//! - 每个 object 必须 `additionalProperties: false`
//! - 定义引用放在 `$defs`

use schemars::gen::SchemaSettings;
use serde_json::{json, Value};

use phi_core::model::OpportunityCard;

pub fn opportunity_card_schema() -> Value {
    let settings = SchemaSettings::draft07().with(|s| {
        s.definitions_path = "#/$defs/".to_owned();
        s.option_add_null_type = true;
        s.inline_subschemas = false;
    });
    let gen = settings.into_generator();
    let schema = gen.into_root_schema_for::<OpportunityCard>();

    let mut v = serde_json::to_value(schema).expect("schema 序列化失败");

    if let Some(obj) = v.as_object_mut() {
        obj.remove("$schema");
        if let Some(defs) = obj.remove("definitions") {
            obj.insert("$defs".to_string(), defs);
        }
    }

    enforce_strict(&mut v);
    v
}

/// 递归地把每个 object 改成严格模式：全部字段 required + 禁止额外字段。
fn enforce_strict(v: &mut Value) {
    match v {
        Value::Object(map) => {
            let is_object_schema = map
                .get("properties")
                .map(|p| p.is_object())
                .unwrap_or(false);

            if is_object_schema {
                let keys: Vec<Value> = map
                    .get("properties")
                    .and_then(|p| p.as_object())
                    .map(|p| p.keys().map(|k| json!(k)).collect())
                    .unwrap_or_default();
                map.insert("required".to_string(), Value::Array(keys));
                map.insert("additionalProperties".to_string(), json!(false));
            }

            let children: Vec<&mut Value> = map
                .iter_mut()
                .filter(|(k, _)| k.as_str() != "required")
                .map(|(_, val)| val)
                .collect();
            for c in children {
                enforce_strict(c);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                enforce_strict(item);
            }
        }
        _ => {}
    }
}

/// 组装成 `response_format` 需要的形状。
pub fn response_format() -> Value {
    json!({
        "type": "json_schema",
        "json_schema": {
            "name": "opportunity_card",
            "strict": true,
            "schema": opportunity_card_schema(),
        }
    })
}

/// 把 schema 里出现的 definitions 也一起检查过 —— 方便调试时直接看一眼。
pub fn pretty() -> String {
    serde_json::to_string_pretty(&opportunity_card_schema()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    fn walk_objects(v: &Value, f: &mut impl FnMut(&Map<String, Value>)) {
        match v {
            Value::Object(m) => {
                if m.contains_key("properties") {
                    f(m);
                }
                for val in m.values() {
                    walk_objects(val, f);
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk_objects(x, f)),
            _ => {}
        }
    }

    #[test]
    fn every_object_is_strict() {
        let s = opportunity_card_schema();
        let mut checked = 0;
        walk_objects(&s, &mut |m| {
            checked += 1;
            assert_eq!(
                m.get("additionalProperties"),
                Some(&json!(false)),
                "缺 additionalProperties: false"
            );
            let props = m["properties"].as_object().unwrap();
            let required = m
                .get("required")
                .and_then(|r| r.as_array())
                .expect("缺 required");
            assert_eq!(
                props.len(),
                required.len(),
                "required 没有覆盖全部 properties"
            );
        });
        assert!(checked > 1, "应当检查到嵌套对象，实际只有 {checked} 个");
    }

    #[test]
    fn has_the_fields_that_matter() {
        let s = opportunity_card_schema();
        let props = s["properties"].as_object().unwrap();
        // 三条硬约束对应的字段一个都不能少
        for k in [
            "pain_evidence",
            "who_pays_evidence",
            "gap_evidence",
            "competitors",
            "trap",
            "buildable",
            "worth_it",
            "reachable",
        ] {
            assert!(props.contains_key(k), "schema 缺字段 {k}");
        }
    }

    #[test]
    fn defs_are_renamed() {
        let s = opportunity_card_schema();
        assert!(s.get("definitions").is_none());
        assert!(s.get("$defs").is_some());
        let text = s.to_string();
        assert!(!text.contains("#/definitions/"), "还有旧的 $ref 路径");
    }
}
