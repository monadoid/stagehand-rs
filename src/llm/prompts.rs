use std::collections::HashMap;

const DEFAULT_OBSERVE_INSTRUCTION: &str = "Find elements that can be used for any future actions in the page. These may be navigation links, related pages, section/subsection links, buttons, or other interactive elements. Be comprehensive: if there are multiple elements that may be relevant for future actions, return all of them.";

pub const SUPPORTED_ACTIONS: &[&str] = &[
    "click",
    "scrollIntoView",
    "scroll",
    "scrollTo",
    "nextChunk",
    "prevChunk",
    "fill",
    "type",
    "press",
    "selectOptionFromDropdown",
];

fn build_user_instructions_string(user_instructions: Option<&str>) -> Option<String> {
    let instructions = user_instructions?.trim();
    if instructions.is_empty() {
        return None;
    }

    Some(format!(
        "\n\n# Custom Instructions Provided by the User\n\nPlease keep the user's instructions in mind when performing actions. If the user's instructions are not relevant to the current task, ignore them.\n\nUser Instructions:\n{instructions}"
    ))
}

pub fn build_observe_system_prompt(user_instructions: Option<&str>) -> String {
    let base = format!(
        "You are helping the user automate the browser by finding elements based on what the user wants to observe in the page.\n\nYou will be given:\n1. an instruction of elements to observe\n2. a hierarchical accessibility tree showing the semantic structure of the page. The tree is a hybrid of the DOM and the accessibility tree.\n\nReturn an array of elements that match the instruction if they exist, otherwise return an empty array. Whenever suggesting actions, use supported playwright locator methods or preferably one of the following supported actions:\n{}\n\nRespond with JSON: always return a JSON object with an `elements` array describing the matches. Each element must be an object with keys `element_id` (the backend DOM node id as an integer), `description` (a short natural language summary), `method` (one of the supported actions), and `arguments` (an array of strings for the action arguments).",
        SUPPORTED_ACTIONS.join(", ")
    );

    match build_user_instructions_string(user_instructions) {
        Some(extra) => format!("{base}{extra}"),
        None => base,
    }
}

pub fn build_observe_user_message(instruction: &str, tree_elements: &str) -> String {
    format!("instruction: {instruction}\nAccessibility Tree: {tree_elements}")
}

pub fn build_extract_system_prompt(
    is_using_text_extract: bool,
    user_instructions: Option<&str>,
) -> String {
    let content_detail = if is_using_text_extract {
        "A text representation of a webpage to extract information from."
    } else {
        "A list of DOM elements to extract from."
    };

    let mut parts = vec![
        format!(
            "You are extracting content on behalf of a user.\nIf a user asks you to extract a 'list' of information, or 'all' information,\nYOU MUST EXTRACT ALL OF THE INFORMATION THAT THE USER REQUESTS.\n\nYou will be given:\n1. An instruction\n2. {content_detail}"
        ),
        format!(
            "Print the exact text from the {} with all symbols, characters, and endlines as is.\nPrint null or an empty string if no new information is found.",
            if is_using_text_extract {
                "text-rendered webpage"
            } else {
                "DOM+accessibility tree elements"
            }
        ),
        "Respond with JSON: your entire reply must be valid JSON that matches the requested schema or structure.".to_string(),
    ];

    if is_using_text_extract {
        parts.push(
            "Once you are given the text-rendered webpage, you must thoroughly and meticulously analyze it. Be very careful to ensure that you do not miss any important information."
                .to_string(),
        );
    } else {
        parts.push(
            "If a user is attempting to extract links or URLs, you MUST respond with ONLY the IDs of the link elements.\nDo not attempt to extract links directly from the text unless absolutely necessary."
                .to_string(),
        );
    }

    if let Some(extra) = build_user_instructions_string(user_instructions) {
        parts.push(extra);
    }

    parts.join("\n\n")
}

pub fn build_extract_user_prompt(instruction: &str, tree_elements: &str) -> String {
    format!("Instruction: {instruction}\nDOM+accessibility tree: {tree_elements}")
}

pub fn build_act_observe_prompt(
    action: &str,
    supported_actions: &[&str],
    variables: Option<&HashMap<String, String>>,
) -> String {
    let mut prompt = format!(
        "Find the most relevant element to perform an action on given the following action: {action}.\nProvide an action for this element such as {}, or any other playwright locator method. Remember that to users, buttons and links look the same in most cases.\nIf the action is completely unrelated to a potential action to be taken on the page, return an empty array.\nONLY return one action. If multiple actions are relevant, return the most relevant one.",
        supported_actions.join(", ")
    );

    if let Some(vars) = variables {
        if !vars.is_empty() {
            prompt.push_str("\n\nAvailable variables:");
            for (name, value) in vars {
                prompt.push_str(&format!("\n- %{name}%: {value}"));
            }
        }
    }

    prompt
}

pub fn effective_observe_instruction(user_instruction: Option<&str>) -> &str {
    match user_instruction {
        Some(value) if !value.trim().is_empty() => value,
        _ => DEFAULT_OBSERVE_INSTRUCTION,
    }
}
