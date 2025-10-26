use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_openai::types::{
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageArgs,
    ChatCompletionRequestSystemMessageContent, ChatCompletionRequestUserMessageArgs,
    ChatCompletionRequestUserMessageContent, ResponseFormat, ResponseFormatJsonSchema,
};
use jsonschema::JSONSchema;
use serde_json::{Value as JsonValue, json};

use crate::a11y::{
    AccessibilityError, AccessibilityPage, get_accessibility_tree, get_xpath_by_resolved_object_id,
};
use crate::browser::BrowserRuntime;
use crate::client::StagehandClientError;
use crate::llm::{ChatCompletionOptions, prompts};
use crate::logging::StagehandLogger;
use crate::page::StagehandPage;
use crate::runtime::ChromiumoxideRuntime;
use crate::types::page::{
    ActOptions, ActResult, DefaultExtractSchema, ExtractOptions, ExtractResult,
    ObserveElementSchema, ObserveOptions, ObserveResult,
};

fn map_accessibility_error(err: AccessibilityError) -> StagehandClientError {
    StagehandClientError::Cdp(err.to_string())
}

fn observe_response_format() -> ResponseFormat {
    ResponseFormat::JsonSchema {
        json_schema: ResponseFormatJsonSchema {
            description: None,
            name: "observe_inference".to_string(),
            schema: Some(json!({
                "type": "object",
                "properties": {
                    "elements": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "element_id": { "type": "integer" },
                                "description": { "type": "string" },
                                "method": { "type": "string" },
                                "arguments": {
                                    "type": "array",
                                    "items": { "type": "string" }
                                }
                            },
                            "required": ["element_id", "description", "method", "arguments"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["elements"],
                "additionalProperties": false
            })),
            strict: Some(false),
        },
    }
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
    chat_options.temperature = Some(0.1);
    chat_options.response_format = Some(observe_response_format());
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
    chat_options.temperature = Some(0.1);

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

    let data = coerce_extract_data(data, options.schema_definition.as_ref(), logger.as_ref());

    Ok(ExtractResult { data: Some(data) })
}

pub async fn act_local(
    page: &StagehandPage<'_, Arc<ChromiumoxideRuntime>>,
    options: ActOptions,
) -> Result<ActResult, StagehandClientError> {
    page.ensure_injection().await?;

    let mut current_options = options;
    let original_options = current_options.clone();
    let original_action = original_options.action.clone();

    let timeout = current_options
        .dom_settle_timeout_ms
        .or(page.client().config().dom_settle_timeout_ms);

    let logger = page.client().logger();
    let max_attempts = if page.client().config().self_heal {
        2
    } else {
        1
    };
    let mut attempt = 0usize;

    loop {
        if attempt == 0 {
            logger.info(
                format!(
                    "Starting action for task: '{}'",
                    current_options.action.as_str()
                ),
                Some("act"),
                None,
            );
        } else {
            logger.info(
                format!(
                    "Self-heal attempt {} for task: '{}'",
                    attempt + 1,
                    current_options.action.as_str()
                ),
                Some("act"),
                None,
            );
        }

        let prompt = prompts::build_act_observe_prompt(
            &current_options.action,
            prompts::SUPPORTED_ACTIONS,
            current_options.variables.as_ref(),
        );

        let observe_options = ObserveOptions {
            instruction: prompt,
            model_name: current_options.model_name.clone(),
            draw_overlay: Some(false),
            dom_settle_timeout_ms: current_options.dom_settle_timeout_ms,
            model_client_options: current_options.model_client_options.clone(),
        };

        let mut results = observe_local(page, observe_options, true).await?;

        if results.is_empty() {
            return Ok(ActResult {
                success: false,
                message: "No observe results found for action".to_string(),
                action: original_action.clone(),
            });
        }

        let mut selected = results.remove(0);
        if let Some(vars) = current_options.variables.as_ref() {
            if let Some(arguments) = selected.arguments.as_mut() {
                *arguments = substitute_variables(arguments, vars);
            }
        }

        let navigation_snapshot = match capture_navigation_snapshot(page).await {
            Ok(snapshot) => Some(snapshot),
            Err(err) => {
                logger.debug(
                    format!("Failed to capture navigation snapshot: {err}"),
                    Some("act"),
                    None,
                );
                None
            }
        };

        let method_label = selected
            .method
            .clone()
            .unwrap_or_else(|| "unknown".to_string());

        match perform_action(page, &selected).await {
            Ok(_) => {
                handle_navigation_after_action(
                    page,
                    navigation_snapshot.as_ref(),
                    timeout,
                    &method_label,
                    &selected.selector,
                )
                .await;

                let action_description = if selected.description.trim().is_empty() {
                    "observe action".to_string()
                } else {
                    selected.description.clone()
                };

                return Ok(ActResult {
                    success: true,
                    message: format!(
                        "Action [{}] performed successfully on selector: {}",
                        method_label, selected.selector
                    ),
                    action: action_description,
                });
            }
            Err(err) => {
                if attempt + 1 >= max_attempts {
                    return Ok(ActResult {
                        success: false,
                        message: format!("Failed to perform act: {err}"),
                        action: original_action.clone(),
                    });
                }

                let Some(fallback_command) = build_self_heal_command(&selected) else {
                    logger.error(
                        "Self-heal attempt aborted: could not construct a fallback command",
                        Some("act"),
                        None,
                    );
                    return Ok(ActResult {
                        success: false,
                        message: format!("Failed to perform act: {err}"),
                        action: original_action.clone(),
                    });
                };

                if original_action
                    .trim()
                    .eq_ignore_ascii_case(fallback_command.trim())
                {
                    logger.error(
                        "Self-heal attempt aborted: fallback command matches original action",
                        Some("act"),
                        None,
                    );
                    return Ok(ActResult {
                        success: false,
                        message: format!("Failed to perform act: {err}"),
                        action: original_action.clone(),
                    });
                }

                logger.info(
                    format!("Attempting self-heal with command: '{fallback_command}'"),
                    Some("act"),
                    None,
                );

                current_options = ActOptions {
                    action: fallback_command,
                    variables: None,
                    ..original_options.clone()
                };
                attempt += 1;
                continue;
            }
        }
    }
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

fn coerce_extract_data(
    data: JsonValue,
    schema: Option<&JsonValue>,
    logger: &StagehandLogger,
) -> JsonValue {
    if let Some(schema_value) = schema {
        match validate_against_schema(schema_value, &data) {
            Ok(()) => data,
            Err(first_error) => {
                logger.debug(
                    format!("Schema validation failed for extract data: {first_error}"),
                    Some("extract"),
                    None,
                );

                let normalized = convert_keys_to_snake_case(&data);
                match validate_against_schema(schema_value, &normalized) {
                    Ok(()) => {
                        logger.debug(
                            "Schema validation succeeded after camelCase normalization",
                            Some("extract"),
                            None,
                        );
                        normalized
                    }
                    Err(second_error) => {
                        logger.error(
                            format!(
                                "Failed to validate extract data against schema: {first_error}. Normalization retry also failed: {second_error}. Returning raw data."
                            ),
                            Some("extract"),
                            None,
                        );
                        data
                    }
                }
            }
        }
    } else {
        match serde_json::from_value::<DefaultExtractSchema>(data.clone()) {
            Ok(model) => serde_json::to_value(model).unwrap_or(data),
            Err(_) => data,
        }
    }
}

#[derive(Default)]
struct NavigationSnapshot {
    url: Option<String>,
    page_ids: HashSet<String>,
    frame_ids: HashMap<String, String>,
}

async fn capture_navigation_snapshot(
    page: &StagehandPage<'_, Arc<ChromiumoxideRuntime>>,
) -> Result<NavigationSnapshot, StagehandClientError> {
    let url_value = page
        .evaluate_expression("(() => window.location.href || '')()")
        .await?;
    let current_url = url_value
        .as_str()
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string());

    let page_ids = page
        .client()
        .browser()
        .runtime()
        .list_pages()
        .await
        .map_err(StagehandClientError::Browser)?
        .into_iter()
        .collect::<HashSet<_>>();

    let mut frame_ids = HashMap::new();
    let page_id_list: Vec<String> = page_ids.iter().cloned().collect();
    for page_id in page_id_list {
        if let Some(frame_id) = page.client().runtime_main_frame_id_for(&page_id).await? {
            frame_ids.insert(page_id, frame_id);
        }
    }

    Ok(NavigationSnapshot {
        url: current_url,
        page_ids,
        frame_ids,
    })
}

async fn handle_navigation_after_action(
    page: &StagehandPage<'_, Arc<ChromiumoxideRuntime>>,
    before: Option<&NavigationSnapshot>,
    timeout_ms: Option<u64>,
    method: &str,
    selector: &str,
) {
    let logger = page.client().logger();
    logger.info(
        format!("{method} action complete; checking for navigation"),
        Some("act"),
        Some(json!({ "selector": selector })),
    );

    if let Err(err) = page.wait_for_settled_dom(timeout_ms).await {
        logger.debug(
            format!("wait_for_settled_dom hit an error after {method}: {err}"),
            Some("act"),
            None,
        );
    }

    let Some(previous) = before else {
        return;
    };

    let after = match capture_navigation_snapshot(page).await {
        Ok(snapshot) => snapshot,
        Err(err) => {
            logger.debug(
                format!("Failed to capture post-action navigation snapshot: {err}"),
                Some("act"),
                None,
            );
            return;
        }
    };

    if previous.url != after.url {
        if let Some(url) = after.url.as_ref() {
            logger.info(
                format!("Navigation detected after {method}: {url}"),
                Some("act"),
                Some(json!({ "selector": selector })),
            );
        }
    }

    let new_pages: Vec<_> = after
        .page_ids
        .difference(&previous.page_ids)
        .cloned()
        .collect();

    for page_id in new_pages {
        logger.info(
            format!("New page detected after {method}: {page_id}"),
            Some("act"),
            None,
        );

        let lock = page.client().page_switch_lock();
        let _guard = lock.lock().await;

        if let Err(err) = page.client().ensure_page_ready(page_id.clone(), None).await {
            logger.error(
                format!("Failed to register new page {page_id}: {err}"),
                Some("act"),
                None,
            );
        } else if let Err(err) = page
            .client()
            .with_context(|ctx| {
                let page_id = page_id.clone();
                Box::pin(async move {
                    ctx.set_active_page(&page_id)?;
                    Ok(())
                })
            })
            .await
        {
            logger.error(
                format!("Failed to set active page after {method}: {err}"),
                Some("act"),
                None,
            );
        }
    }

    if let Some(previous) = before {
        for (page_id, frame_id) in &after.frame_ids {
            let needs_update = previous
                .frame_ids
                .get(page_id)
                .map(|existing| existing != frame_id)
                .unwrap_or(true);
            if needs_update {
                if let Err(err) = page
                    .client()
                    .ensure_frame_registered(page_id, Some(frame_id.clone()))
                    .await
                {
                    logger.debug(
                        format!("Failed to refresh frame id for {page_id}: {err}"),
                        Some("act"),
                        None,
                    );
                }
            }
        }
    }
}

fn build_self_heal_command(result: &ObserveResult) -> Option<String> {
    let method = result.method.as_deref().unwrap_or("").trim();
    let description = result.description.trim();

    if method.is_empty() && description.is_empty() {
        return None;
    }

    if !method.is_empty()
        && !description.is_empty()
        && description
            .to_ascii_lowercase()
            .starts_with(&method.to_ascii_lowercase())
    {
        return Some(description.to_string());
    }

    if !method.is_empty() {
        let combined = format!("{method} {description}").trim().to_string();
        if !combined.is_empty() {
            return Some(combined);
        }
    }

    if !description.is_empty() {
        return Some(description.to_string());
    }

    None
}

fn validate_against_schema(schema: &JsonValue, data: &JsonValue) -> Result<(), String> {
    let compiled = JSONSchema::compile(schema).map_err(|err| err.to_string())?;
    if let Err(errors) = compiled.validate(data) {
        let joined = errors
            .map(|issue| issue.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(joined);
    }
    Ok(())
}

fn convert_keys_to_snake_case(value: &JsonValue) -> JsonValue {
    match value {
        JsonValue::Object(map) => {
            let mut converted = serde_json::Map::with_capacity(map.len());
            for (key, val) in map {
                let normalized_key = camel_to_snake_case(key);
                converted.insert(normalized_key, convert_keys_to_snake_case(val));
            }
            JsonValue::Object(converted)
        }
        JsonValue::Array(items) => JsonValue::Array(
            items
                .iter()
                .map(|item| convert_keys_to_snake_case(item))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn camel_to_snake_case(input: &str) -> String {
    let mut result = String::new();
    let mut prev: Option<char> = None;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch.is_ascii_uppercase() {
            let next_is_lowercase = chars
                .peek()
                .map(|next| next.is_ascii_lowercase())
                .unwrap_or(false);
            let prev_is_lowercase_or_digit = prev
                .map(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                .unwrap_or(false);

            if !result.is_empty() && (prev_is_lowercase_or_digit || next_is_lowercase) {
                if !result.ends_with('_') {
                    result.push('_');
                }
            }
            result.push(ch.to_ascii_lowercase());
        } else if ch == '-' {
            if !result.ends_with('_') {
                result.push('_');
            }
        } else {
            result.push(ch);
        }
        prev = Some(ch);
    }

    result
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
    use crate::config::Verbosity;

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

    #[test]
    fn build_self_heal_command_prefers_existing_description() {
        let result = ObserveResult {
            selector: "xpath=//button".to_string(),
            description: "click Submit button".to_string(),
            backend_node_id: Some(1),
            method: Some("click".to_string()),
            arguments: None,
        };

        let command = build_self_heal_command(&result);
        assert_eq!(command.as_deref(), Some("click Submit button"));
    }

    #[test]
    fn convert_keys_to_snake_case_handles_nested_objects() {
        let value = json!({
            "companyName": "Stagehand",
            "details": {
                "headquartersCity": "San Francisco"
            },
            "items": [
                { "externalUrl": "1" }
            ]
        });

        let normalized = convert_keys_to_snake_case(&value);
        assert!(normalized.get("company_name").is_some());
        assert!(
            normalized
                .get("details")
                .and_then(|details| details.get("headquarters_city"))
                .is_some()
        );
        assert!(
            normalized
                .get("items")
                .and_then(|items| items.get(0))
                .and_then(|item| item.get("external_url"))
                .is_some()
        );
    }

    #[test]
    fn coerce_extract_data_normalizes_and_validates_against_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "company_name": { "type": "string" }
            },
            "required": ["company_name"],
            "additionalProperties": false
        });

        let logger = StagehandLogger::new(Verbosity::Detailed);
        let input = json!({ "companyName": "Stagehand" });
        let result = coerce_extract_data(input, Some(&schema), &logger);

        assert_eq!(
            result["company_name"],
            JsonValue::String("Stagehand".to_string())
        );
    }
}
