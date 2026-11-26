//! Pure-data declarative UI Mod node protocol and tree validation.
//!
//! Zero terminal/graphics dependencies: hosts (TUI, native GUI, web, headless) decide how to
//! render [`UiNode`] trees, while headless hosts simply ignore UI registrations with zero overhead.

use crate::types::JsExtensionError;
use serde::{Deserialize, Serialize};

/// Maximum allowed nesting depth of a [`UiNode`] tree (root is depth 1).
pub const MAX_UI_NODE_DEPTH: usize = 16;

/// Maximum allowed total node count in a [`UiNode`] tree.
pub const MAX_UI_NODE_COUNT: usize = 256;

/// Maximum allowed byte length of a single [`UiNode::Text`] string.
pub const MAX_UI_TEXT_LEN: usize = 4_096;

/// Maximum allowed byte length of a [`UiNode::Divider`] character sequence.
pub const MAX_UI_DIVIDER_CH_LEN: usize = 16;

/// Maximum allowed byte length of a color/background token string.
pub const MAX_UI_TOKEN_LEN: usize = 64;

/// Maximum allowed gap (rows or columns) on a [`UiNode::Box`].
pub const MAX_UI_BOX_GAP: u16 = 16;

/// Maximum allowed left/right padding (columns) on a [`UiNode::Box`].
pub const MAX_UI_BOX_PADDING: u16 = 32;

/// Recommended default per-render timeout (in milliseconds) for interactive UI hosts.
pub const DEFAULT_UI_RENDER_TIMEOUT_MS: u64 = 40;

/// UI component slots that JS extensions can intercept via `jimu.on("ui.render", (e, next) => ...)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UiComponent {
    StatusLine,
    AbovePrompt,
    Header,
    Footer,
    UserMessage,
    ToolRow,
    Spinner,
}

impl UiComponent {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StatusLine => "StatusLine",
            Self::AbovePrompt => "AbovePrompt",
            Self::Header => "Header",
            Self::Footer => "Footer",
            Self::UserMessage => "UserMessage",
            Self::ToolRow => "ToolRow",
            Self::Spinner => "Spinner",
        }
    }
}

/// Flex layout direction for [`UiNode::Box`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiDirection {
    Row,
    #[default]
    Column,
}

/// Border style for [`UiNode::Box`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiBorder {
    #[default]
    None,
    Left,
    Top,
    Bottom,
    All,
}

/// Text styling attributes flattened into [`UiNode::Text`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiTextStyle {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dim: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub italic: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underline: Option<bool>,
}

/// Pure-data declarative UI node tree returned by `ui.render` interceptors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiNode {
    /// Host native rendering (returned by the innermost `next(e)` call).
    Native,
    /// Styled text leaf node.
    Text {
        text: String,
        #[serde(default, flatten)]
        style: UiTextStyle,
    },
    /// Container node arranging children in a row or column.
    Box {
        #[serde(default)]
        direction: UiDirection,
        #[serde(default)]
        gap: u16,
        #[serde(default)]
        border: UiBorder,
        #[serde(
            default,
            alias = "borderColor",
            skip_serializing_if = "Option::is_none"
        )]
        border_color: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bg: Option<String>,
        #[serde(default, alias = "paddingLeft")]
        padding_left: u16,
        #[serde(default, alias = "paddingRight")]
        padding_right: u16,
        #[serde(default)]
        children: Vec<UiNode>,
    },
    /// Horizontal divider line.
    Divider {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ch: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<String>,
    },
}

/// Validate a [`UiNode`] tree against hard structural and size limits so hosts can safely
/// fall back to native rendering when an extension produces an oversized or malformed tree.
pub fn validate_ui_node(node: &UiNode) -> Result<(), String> {
    let mut count = 0usize;
    validate_node_rec(node, 1, &mut count)
}

fn validate_token(field: &str, token: Option<&str>) -> Result<(), String> {
    if let Some(value) = token
        && value.len() > MAX_UI_TOKEN_LEN
    {
        return Err(format!(
            "UI style token '{field}' exceeds maximum length of {MAX_UI_TOKEN_LEN} bytes"
        ));
    }
    Ok(())
}

fn validate_node_rec(node: &UiNode, depth: usize, count: &mut usize) -> Result<(), String> {
    if depth > MAX_UI_NODE_DEPTH {
        return Err(format!(
            "UI node tree exceeds maximum depth of {MAX_UI_NODE_DEPTH}"
        ));
    }
    *count += 1;
    if *count > MAX_UI_NODE_COUNT {
        return Err(format!(
            "UI node tree exceeds maximum node count of {MAX_UI_NODE_COUNT}"
        ));
    }

    match node {
        UiNode::Native => Ok(()),
        UiNode::Text { text, style } => {
            if text.len() > MAX_UI_TEXT_LEN {
                return Err(format!(
                    "UI text node exceeds maximum length of {MAX_UI_TEXT_LEN} bytes"
                ));
            }
            validate_token("color", style.color.as_deref())?;
            validate_token("bg", style.bg.as_deref())?;
            Ok(())
        }
        UiNode::Box {
            gap,
            border_color,
            bg,
            padding_left,
            padding_right,
            children,
            ..
        } => {
            if *gap > MAX_UI_BOX_GAP {
                return Err(format!("UI box gap exceeds maximum of {MAX_UI_BOX_GAP}"));
            }
            if *padding_left > MAX_UI_BOX_PADDING {
                return Err(format!(
                    "UI box padding_left exceeds maximum of {MAX_UI_BOX_PADDING}"
                ));
            }
            if *padding_right > MAX_UI_BOX_PADDING {
                return Err(format!(
                    "UI box padding_right exceeds maximum of {MAX_UI_BOX_PADDING}"
                ));
            }
            validate_token("border_color", border_color.as_deref())?;
            validate_token("bg", bg.as_deref())?;
            for child in children {
                validate_node_rec(child, depth + 1, count)?;
            }
            Ok(())
        }
        UiNode::Divider { ch, color } => {
            if let Some(ch) = ch
                && ch.len() > MAX_UI_DIVIDER_CH_LEN
            {
                return Err(format!(
                    "UI divider character exceeds maximum length of {MAX_UI_DIVIDER_CH_LEN} bytes"
                ));
            }
            validate_token("color", color.as_deref())?;
            Ok(())
        }
    }
}

pub(crate) fn parse_and_validate_ui_node(json: &str) -> Result<UiNode, JsExtensionError> {
    let node: UiNode = serde_json::from_str(json)
        .map_err(|err| JsExtensionError::InvalidUiNode(format!("invalid UI node JSON: {err}")))?;
    validate_ui_node(&node).map_err(JsExtensionError::InvalidUiNode)?;
    Ok(node)
}
