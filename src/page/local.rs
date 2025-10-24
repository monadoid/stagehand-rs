use std::collections::HashMap;
use std::sync::Arc;

use async_openai::types::{
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageArgs,
    ChatCompletionRequestSystemMessageContent, ChatCompletionRequestUserMessageArgs,
    ChatCompletionRequestUserMessageContent, ResponseFormat, ResponseFormatJsonSchema,
};
use serde_json::{Value as JsonValue, json};

use crate::a11y::{
    AccessibilityError, AccessibilityPage, get_accessibility_tree, get_xpath_by_resolved_object_id,
};
use crate::client::StagehandClientError;
use crate::llm::{ChatCompletionOptions, prompts};
use crate::page::StagehandPage;
use crate::runtime::ChromiumoxideRuntime;
use crate::types::page::{
    ActOptions, ActResult, DefaultExtractSchema, ExtractOptions, ExtractResult,
    ObserveElementSchema, ObserveOptions, ObserveResult,
};

fn map_accessibility_error(err: AccessibilityError) -> StagehandClientError {
    StagehandClientError::Cdp(err.to_string())
}

pub async fn observe_local(
    page: &StagehandPage<'_, Arc<ChromiumoxideRuntime>>,
    mut options: ObserveOptions,
    from_act: bool,
) -> Result<Vec<ObserveResult>, StagehandClientError> {
    page.ensure_injection().await?;
    let timeout = options
        .dom_settle_timeout_ms
        .or(page.client().config().dom_settle_timeout_ms);
    page.wait_for_settled_dom(timeout).await?;

    let logger = page.client().logger();
    let tree = get_accessibility_tree(page, logger.as_ref())
        .await
        .map_err(map_accessibility_error)?;

    let instruction = if options.instruction.trim().is_empty() {
        prompts::effective_observe_instruction(None).to_string()
    } else {
        options.instruction.clone()
    };

    logger.info(
        format!("Starting observation for task: '{instruction}'"),
        Some("observe"),
        None,
    );

    let system_message = prompts::build_observe_system_prompt(None);
    let user_message = prompts::build_observe_user_message(&instruction, &tree.simplified);

    let system = ChatCompletionRequestMessage::System(
        ChatCompletionRequestSystemMessageArgs::default()
            .content(ChatCompletionRequestSystemMessageContent::Text(
                system_message,
            ))
            .build()
            .map_err(|err| StagehandClientError::Api(err.to_string()))?,
    );

    let user = ChatCompletionRequestMessage::User(
        ChatCompletionRequestUserMessageArgs::default()
            .content(ChatCompletionRequestUserMessageContent::Text(user_message))
            .build()
            .map_err(|err| StagehandClientError::Api(err.to_string()))?,
    );

    let mut chat_options = ChatCompletionOptions::default();
    chat_options.model = options.model_name.clone();
    chat_options.response_format = Some(ResponseFormat::JsonObject);
    chat_options.metadata = options.model_client_options.take();

    let llm = page.client().create_llm_client()?;
    let response = llm
        .create_chat_completion(
            vec![system, user],
            chat_options,
            Some(if from_act { "ACT" } else { "OBSERVE" }),
        )
        .await?;

    let content = response
        .choices
        .first()
        .and_then(|choice| choice.message.content.clone())
        .unwrap_or_default();

    logger.debug(
        "LLM observe response",
        Some("observe"),
        Some(json!({ "content": content })),
    );

    let mut elements: Vec<ObserveElementSchema> =
        match serde_json::from_str::<Vec<ObserveElementSchema>>(&content) {
            Ok(list) => list,
            Err(_) => match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(value) => value
                    .get("elements")
                    .and_then(|value| serde_json::from_value(value.clone()).ok())
                    .unwrap_or_default(),
                Err(err) => {
                    logger.error(
                        format!("Failed to parse observe response: {err}"),
                        Some("observe"),
                        None,
                    );
                    Vec::new()
                }
            },
        };

    for iframe in &tree.iframes {
        if let Some(node_id) = iframe.node_id.as_ref() {
            if let Ok(element_id) = node_id.parse::<i64>() {
                elements.push(ObserveElementSchema {
                    element_id,
                    description: "an iframe".to_string(),
                    method: "not-supported".to_string(),
                    arguments: Vec::new(),
                });
            }
        }
    }

    let results = attach_selectors(page, elements).await?;

    if options.draw_overlay.unwrap_or(false) && !results.is_empty() {
        if let Err(err) = draw_observe_overlay(page, &results).await {
            page.client()
                .log_debug(&format!("Failed to draw observe overlay: {err}"), "observe");
        }
    }

    Ok(results)
}

async fn draw_observe_overlay(
    page: &StagehandPage<'_, Arc<ChromiumoxideRuntime>>,
    results: &[ObserveResult],
) -> Result<(), StagehandClientError> {
    let payload = serde_json::to_string(results)?;
    let script = format!(
        "(function(elementsJson) {{
            try {{
                const elements = JSON.parse(elementsJson || '[]');
                document.querySelectorAll('.stagehand-observe-overlay').forEach(el => el.remove());
                if (!Array.isArray(elements) || elements.length === 0) {{
                    return true;
                }}

                const container = document.createElement('div');
                container.className = 'stagehand-observe-overlay';
                container.style.position = 'fixed';
                container.style.top = '0';
                container.style.left = '0';
                container.style.width = '100%';
                container.style.height = '100%';
                container.style.pointerEvents = 'none';
                container.style.zIndex = '999999';
                document.body.appendChild(container);

                elements.forEach(function(element, index) {{
                    const selector = element && element.selector ? String(element.selector) : '';
                    if (!selector) {{
                        return;
                    }}

                    let target = null;
                    if (selector.startsWith('xpath=')) {{
                        const expression = selector.slice(6);
                        const result = document.evaluate(expression, document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null);
                        target = result.singleNodeValue;
                    }} else {{
                        target = document.querySelector(selector);
                    }}

                    if (!target || !(target instanceof Element)) {{
                        return;
                    }}

                    const rect = target.getBoundingClientRect();
                    const overlay = document.createElement('div');
                    overlay.style.position = 'absolute';
                    overlay.style.left = rect.left + 'px';
                    overlay.style.top = rect.top + 'px';
                    overlay.style.width = rect.width + 'px';
                    overlay.style.height = rect.height + 'px';
                    overlay.style.border = '2px solid #ff4d4f';
                    overlay.style.backgroundColor = 'rgba(255, 77, 79, 0.15)';
                    overlay.style.boxSizing = 'border-box';
                    overlay.style.pointerEvents = 'none';

                    const label = document.createElement('div');
                    label.textContent = String(index + 1);
                    label.style.position = 'absolute';
                    label.style.left = '0';
                    label.style.top = '-20px';
                    label.style.backgroundColor = '#ff4d4f';
                    label.style.color = '#fff';
                    label.style.padding = '2px 4px';
                    label.style.fontSize = '12px';
                    label.style.borderRadius = '3px';

                    overlay.appendChild(label);
                    container.appendChild(overlay);
                }});

                setTimeout(() => {{
                    document.querySelectorAll('.stagehand-observe-overlay').forEach(el => el.remove());
                }}, 5000);
            }} catch (error) {{
                console.error('Stagehand overlay error', error);
            }}
            return true;
        }})({payload});",
    );

    page.evaluate_expression(&script).await?;
    Ok(())
}

pub async fn extract_local(
    page: &StagehandPage<'_, Arc<ChromiumoxideRuntime>>,
    mut options: ExtractOptions,
) -> Result<ExtractResult, StagehandClientError> {
    page.ensure_injection().await?;
    let timeout = options
        .dom_settle_timeout_ms
        .or(page.client().config().dom_settle_timeout_ms);
    page.wait_for_settled_dom(timeout).await?;

    let logger = page.client().logger();
    let tree = get_accessibility_tree(page, logger.as_ref())
        .await
        .map_err(map_accessibility_error)?;

    let instruction = options.instruction.clone();
    logger.info(
        format!("Starting extraction with instruction: '{instruction}'"),
        Some("extract"),
        None,
    );

    let system_prompt =
        prompts::build_extract_system_prompt(options.use_text_extract.unwrap_or(false), None);
    let user_prompt = prompts::build_extract_user_prompt(&instruction, &tree.simplified);

    let system = ChatCompletionRequestMessage::System(
        ChatCompletionRequestSystemMessageArgs::default()
            .content(ChatCompletionRequestSystemMessageContent::Text(
                system_prompt,
            ))
            .build()
            .map_err(|err| StagehandClientError::Api(err.to_string()))?,
    );

    let user = ChatCompletionRequestMessage::User(
        ChatCompletionRequestUserMessageArgs::default()
            .content(ChatCompletionRequestUserMessageContent::Text(user_prompt))
            .build()
            .map_err(|err| StagehandClientError::Api(err.to_string()))?,
    );

    let mut chat_options = ChatCompletionOptions::default();
    chat_options.model = options.model_name.clone();
    chat_options.metadata = options.model_client_options.take();

    chat_options.response_format =
        build_extract_response_format(options.schema_definition.as_ref())
            .or(Some(ResponseFormat::JsonObject));

    let llm = page.client().create_llm_client()?;
    let response = llm
        .create_chat_completion(vec![system, user], chat_options, Some("EXTRACT"))
        .await?;

    let content = response
        .choices
        .first()
        .and_then(|choice| choice.message.content.clone())
        .unwrap_or_default();

    logger.debug(
        "LLM extract response",
        Some("extract"),
        Some(json!({ "content": content })),
    );

    let parsed: JsonValue = match serde_json::from_str(&content) {
        Ok(value) => value,
        Err(err) => {
            logger.error(
                format!("Failed to parse extract response: {err}"),
                Some("extract"),
                None,
            );
            return Ok(ExtractResult { data: None });
        }
    };

    let mut data = parsed
        .get("data")
        .cloned()
        .unwrap_or_else(|| parsed.clone());

    if !tree.id_to_url.is_empty() {
        inject_urls(&mut data, &tree.id_to_url);
    }

    let data = coerce_extract_data(data, options.schema_definition.as_ref());

    Ok(ExtractResult { data: Some(data) })
}

pub async fn act_local(
    page: &StagehandPage<'_, Arc<ChromiumoxideRuntime>>,
    mut options: ActOptions,
) -> Result<ActResult, StagehandClientError> {
    page.ensure_injection().await?;

    let timeout = options
        .dom_settle_timeout_ms
        .or(page.client().config().dom_settle_timeout_ms);

    let logger = page.client().logger();
    logger.info(
        format!("Starting action for task: '{}'", options.action),
        Some("act"),
        None,
    );

    let prompt = prompts::build_act_observe_prompt(
        &options.action,
        prompts::SUPPORTED_ACTIONS,
        options.variables.as_ref(),
    );

    let observe_options = ObserveOptions {
        instruction: prompt,
        model_name: options.model_name.clone(),
        draw_overlay: Some(false),
        dom_settle_timeout_ms: options.dom_settle_timeout_ms,
        model_client_options: options.model_client_options.take(),
    };

    let mut results = observe_local(page, observe_options, true).await?;

    if results.is_empty() {
        return Ok(ActResult {
            success: false,
            message: "No observe results found for action".to_string(),
            action: options.action,
        });
    }

    let mut selected = results.remove(0);
    if let Some(vars) = options.variables.as_ref() {
        if let Some(arguments) = selected.arguments.as_mut() {
            *arguments = substitute_variables(arguments, vars);
        }
    }

    perform_action(page, &selected).await?;

    page.wait_for_settled_dom(timeout).await?;

    let action_description = if selected.description.trim().is_empty() {
        "observe action".to_string()
    } else {
        selected.description.clone()
    };

    Ok(ActResult {
        success: true,
        message: format!(
            "Action [{}] performed successfully on selector: {}",
            selected
                .method
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
            selected.selector
        ),
        action: action_description,
    })
}

fn build_extract_response_format(schema: Option<&JsonValue>) -> Option<ResponseFormat> {
    schema.map(|value| ResponseFormat::JsonSchema {
        json_schema: ResponseFormatJsonSchema {
            description: None,
            name: "extraction_schema".to_string(),
            schema: Some(value.clone()),
            strict: Some(false),
        },
    })
}

fn inject_urls(value: &mut JsonValue, id_to_url: &HashMap<String, String>) {
    match value {
        JsonValue::Object(map) => {
            for (key, val) in map.iter_mut() {
                if key.to_ascii_lowercase().contains("url") {
                    replace_with_url(val, id_to_url);
                }
                inject_urls(val, id_to_url);
            }
        }
        JsonValue::Array(items) => {
            for item in items {
                inject_urls(item, id_to_url);
            }
        }
        _ => {}
    }
}

fn replace_with_url(value: &mut JsonValue, id_to_url: &HashMap<String, String>) {
    match value {
        JsonValue::Number(number) => {
            if let Some(id) = number.as_i64() {
                if let Some(url) = id_to_url.get(&id.to_string()) {
                    *value = JsonValue::String(url.clone());
                }
            }
        }
        JsonValue::String(text) => {
            if let Ok(id) = text.trim().parse::<i64>() {
                if let Some(url) = id_to_url.get(&id.to_string()) {
                    *value = JsonValue::String(url.clone());
                }
            }
        }
        _ => {}
    }
}

fn coerce_extract_data(data: JsonValue, schema: Option<&JsonValue>) -> JsonValue {
    if schema.is_some() {
        return data;
    }

    match serde_json::from_value::<DefaultExtractSchema>(data.clone()) {
        Ok(model) => serde_json::to_value(model).unwrap_or(data),
        Err(_) => data,
    }
}

fn substitute_variables(args: &[String], variables: &HashMap<String, String>) -> Vec<String> {
    args.iter()
        .map(|arg| {
            let mut current = arg.clone();
            for (key, value) in variables {
                let needle = format!("%{key}%");
                current = current.replace(&needle, value);
            }
            current
        })
        .collect()
}

async fn attach_selectors(
    page: &StagehandPage<'_, Arc<ChromiumoxideRuntime>>,
    elements: Vec<ObserveElementSchema>,
) -> Result<Vec<ObserveResult>, StagehandClientError> {
    if elements.is_empty() {
        return Ok(Vec::new());
    }

    let session = page
        .new_cdp_session()
        .await
        .map_err(map_accessibility_error)?;

    let mut results = Vec::new();
    for element in elements {
        if element.element_id <= 0 {
            continue;
        }

        let response = page
            .send_cdp(
                "DOM.resolveNode",
                Some(json!({ "backendNodeId": element.element_id })),
            )
            .await?;

        let object_id = response
            .get("object")
            .and_then(|object| object.get("objectId"))
            .and_then(JsonValue::as_str)
            .map(|value| value.to_string());

        let Some(object_id) = object_id else {
            continue;
        };

        let xpath = get_xpath_by_resolved_object_id(session.as_ref(), &object_id).await;

        if xpath.is_empty() {
            continue;
        }

        results.push(ObserveResult {
            selector: format!("xpath={xpath}"),
            description: element.description.clone(),
            backend_node_id: Some(element.element_id),
            method: Some(element.method.clone()),
            arguments: if element.arguments.is_empty() {
                None
            } else {
                Some(element.arguments.clone())
            },
        });
    }

    let _ = session.detach().await;

    Ok(results)
}

async fn perform_action(
    page: &StagehandPage<'_, Arc<ChromiumoxideRuntime>>,
    result: &ObserveResult,
) -> Result<(), StagehandClientError> {
    let original_method = result.method.as_deref().unwrap_or("click");
    let normalized = original_method.trim().to_ascii_lowercase();

    let outcome = match normalized.as_str() {
        "click" => perform_click(page, &result.selector).await,
        "scrollintoview" => scroll_into_view(page, &result.selector).await,
        "scroll" | "scrollto" | "mouse.wheel" => {
            let arg = result
                .arguments
                .as_ref()
                .and_then(|args| args.get(0).map(String::as_str));
            scroll_to_percentage(page, &result.selector, arg).await
        }
        "nextchunk" => scroll_chunk(page, &result.selector, 1).await,
        "prevchunk" => scroll_chunk(page, &result.selector, -1).await,
        "fill" | "type" => {
            let text = result
                .arguments
                .as_ref()
                .and_then(|args| args.get(0))
                .map(String::as_str)
                .unwrap_or_default();
            fill_element(page, &result.selector, text).await
        }
        "press" => {
            let key = result
                .arguments
                .as_ref()
                .and_then(|args| args.get(0))
                .map(String::as_str)
                .unwrap_or_default();
            press_key_action(page, &result.selector, key).await
        }
        "selectoptionfromdropdown" => {
            let value = result
                .arguments
                .as_ref()
                .and_then(|args| args.get(0))
                .map(String::as_str)
                .unwrap_or_default();
            select_option_from_dropdown(page, &result.selector, value).await
        }
        "not-supported" => Err(StagehandClientError::Unsupported(
            "ObserveResult method is not supported in local mode",
        )),
        _ => Err(StagehandClientError::Unsupported(
            "local observe result method not supported",
        )),
    };

    match outcome {
        Ok(_) => {
            page.client().log_debug(
                &format!("Executed local action via {original_method}"),
                "act",
            );
            Ok(())
        }
        Err(err) => {
            page.client().log_error(
                &format!("Failed to execute {original_method}: {err}"),
                "act",
            );
            Err(err)
        }
    }
}

fn ensure_xpath<'a>(selector: &'a str) -> Result<&'a str, StagehandClientError> {
    selector
        .strip_prefix("xpath=")
        .ok_or(StagehandClientError::Unsupported(
            "local handlers currently require xpath selectors",
        ))
}

fn build_xpath_script(xpath: &str, body: &str) -> Result<String, StagehandClientError> {
    let xpath_json = serde_json::to_string(xpath)?;
    Ok(format!(
        "(function() {{
            const result = document.evaluate({xpath}, document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null);
            const el = result.singleNodeValue;
            if (!el) {{
                throw new Error('Element not found for xpath');
            }}
            {body}
        }})()",
        xpath = xpath_json,
        body = body
    ))
}

async fn perform_click(
    page: &StagehandPage<'_, Arc<ChromiumoxideRuntime>>,
    selector: &str,
) -> Result<(), StagehandClientError> {
    let xpath = ensure_xpath(selector)?;
    let script = build_xpath_script(xpath, "el.click(); return true;")?;
    page.evaluate_expression(&script).await?;
    Ok(())
}

async fn scroll_into_view(
    page: &StagehandPage<'_, Arc<ChromiumoxideRuntime>>,
    selector: &str,
) -> Result<(), StagehandClientError> {
    let xpath = ensure_xpath(selector)?;
    let script = build_xpath_script(
        xpath,
        "el.scrollIntoView({ behavior: 'smooth', block: 'center', inline: 'center' }); return true;",
    )?;
    page.evaluate_expression(&script).await?;
    Ok(())
}

async fn scroll_to_percentage(
    page: &StagehandPage<'_, Arc<ChromiumoxideRuntime>>,
    selector: &str,
    percentage: Option<&str>,
) -> Result<(), StagehandClientError> {
    let xpath = ensure_xpath(selector)?;
    let percent = percentage.unwrap_or("0");
    let percent_json = serde_json::to_string(percent)?;
    let body = format!(
        "const parsePercent = (raw) => {{
            if (typeof raw === 'number') return Math.max(0, Math.min(raw, 100));
            const cleaned = String(raw).trim().replace('%', '');
            const value = parseFloat(cleaned);
            if (Number.isNaN(value)) return 0;
            return Math.max(0, Math.min(value, 100));
        }};
        const yPct = parsePercent({percent});
        const tag = el.tagName ? el.tagName.toLowerCase() : '';
        if (tag === 'html' || tag === 'body') {{
            const scrollHeight = Math.max(document.body.scrollHeight - window.innerHeight, 0);
            const top = scrollHeight * (yPct / 100);
            window.scrollTo({{ top, left: window.scrollX, behavior: 'smooth' }});
        }} else {{
            const scrollHeight = Math.max(el.scrollHeight - el.clientHeight, 0);
            const top = scrollHeight * (yPct / 100);
            el.scrollTo({{ top, left: el.scrollLeft, behavior: 'smooth' }});
        }}
        return true;",
        percent = percent_json
    );
    let script = build_xpath_script(xpath, &body)?;
    page.evaluate_expression(&script).await?;
    Ok(())
}

async fn scroll_chunk(
    page: &StagehandPage<'_, Arc<ChromiumoxideRuntime>>,
    selector: &str,
    direction: i32,
) -> Result<(), StagehandClientError> {
    let xpath = ensure_xpath(selector)?;
    let body = format!(
        "const tag = el.tagName ? el.tagName.toLowerCase() : '';
        const delta = (tag === 'html' || tag === 'body')
            ? window.innerHeight * {dir}
            : el.clientHeight * {dir};
        if (tag === 'html' || tag === 'body') {{
            window.scrollBy({{ top: delta, left: 0, behavior: 'smooth' }});
        }} else {{
            el.scrollBy({{ top: delta, left: 0, behavior: 'smooth' }});
        }}
        return true;",
        dir = direction
    );
    let script = build_xpath_script(xpath, &body)?;
    page.evaluate_expression(&script).await?;
    Ok(())
}

async fn fill_element(
    page: &StagehandPage<'_, Arc<ChromiumoxideRuntime>>,
    selector: &str,
    text: &str,
) -> Result<(), StagehandClientError> {
    let xpath = ensure_xpath(selector)?;
    let text_json = serde_json::to_string(text)?;
    let body = format!(
        "const value = {text};
        el.focus();
        if ('value' in el) {{
            el.value = value;
        }}
        el.dispatchEvent(new Event('input', {{ bubbles: true }}));
        el.dispatchEvent(new Event('change', {{ bubbles: true }}));
        return true;",
        text = text_json
    );
    let script = build_xpath_script(xpath, &body)?;
    page.evaluate_expression(&script).await?;
    Ok(())
}

async fn press_key_action(
    page: &StagehandPage<'_, Arc<ChromiumoxideRuntime>>,
    selector: &str,
    key: &str,
) -> Result<(), StagehandClientError> {
    let xpath = ensure_xpath(selector)?;
    let key_json = serde_json::to_string(key)?;
    let body = format!(
        "const keyValue = {key};
        el.focus();
        const eventInit = {{ key: keyValue, bubbles: true, cancelable: true }};
        el.dispatchEvent(new KeyboardEvent('keydown', eventInit));
        el.dispatchEvent(new KeyboardEvent('keyup', eventInit));
        return true;",
        key = key_json
    );
    let script = build_xpath_script(xpath, &body)?;
    page.evaluate_expression(&script).await?;
    Ok(())
}

async fn select_option_from_dropdown(
    page: &StagehandPage<'_, Arc<ChromiumoxideRuntime>>,
    selector: &str,
    value: &str,
) -> Result<(), StagehandClientError> {
    let xpath = ensure_xpath(selector)?;
    let value_json = serde_json::to_string(value)?;
    let body = format!(
        "const desired = {value};
        if (!el || el.tagName.toLowerCase() !== 'select') {{
            throw new Error('Target is not a <select> element');
        }}
        const options = Array.from(el.options);
        const match = options.find(opt => opt.value === desired || opt.text === desired);
        if (!match && desired) {{
            throw new Error('No matching option for value');
        }}
        if (match) {{
            el.value = match.value;
        }}
        el.dispatchEvent(new Event('input', {{ bubbles: true }}));
        el.dispatchEvent(new Event('change', {{ bubbles: true }}));
        return true;",
        value = value_json
    );
    let script = build_xpath_script(xpath, &body)?;
    page.evaluate_expression(&script).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitute_variables_replaces_tokens() {
        let args = vec![
            "click on %TARGET% now".to_string(),
            "value=%VALUE%".to_string(),
        ];
        let mut vars = HashMap::new();
        vars.insert("TARGET".to_string(), "button".to_string());
        vars.insert("VALUE".to_string(), "42".to_string());

        let substituted = substitute_variables(&args, &vars);
        assert_eq!(
            substituted,
            vec!["click on button now".to_string(), "value=42".to_string()]
        );
    }

    #[test]
    fn build_extract_response_format_wraps_schema() {
        let schema = json!({ "type": "object", "properties": { "name": { "type": "string" } } });
        let format = build_extract_response_format(Some(&schema));
        match format {
            Some(ResponseFormat::JsonSchema { json_schema }) => {
                assert_eq!(json_schema.name, "extraction_schema");
                assert_eq!(json_schema.schema, Some(schema));
            }
            other => panic!("unexpected response format: {:?}", other),
        }
    }

    #[test]
    fn inject_urls_replaces_matching_ids() {
        let mut value = json!({
            "url": 42,
            "nested": {
                "items": [
                    { "linkUrl": "7" },
                    { "other": "value" }
                ]
            }
        });
        let mut map = HashMap::new();
        map.insert("42".to_string(), "https://example.com".to_string());
        map.insert("7".to_string(), "https://example.com/inner".to_string());

        inject_urls(&mut value, &map);
        assert_eq!(
            value["url"],
            JsonValue::String("https://example.com".to_string())
        );
        assert_eq!(
            value["nested"]["items"][0]["linkUrl"],
            JsonValue::String("https://example.com/inner".to_string())
        );
    }
}
