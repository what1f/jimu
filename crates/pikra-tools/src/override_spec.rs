use pikra_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolProgressCallback};
use pikra_ai::{BoxFuture, CancelToken, ToolDefinition};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DescriptionOverride {
    Replace(String),
    Append(String),
}

/// Host customizations for the tool's top-level parameters.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ToolDefinitionOverride {
    pub rename: Option<String>,
    pub description: Option<DescriptionOverride>,
    pub hidden_parameters: Vec<String>,
    pub parameter_descriptions: BTreeMap<String, String>,
    /// Defaults for omitted arguments. Defaults on hidden parameters are pinned.
    pub default_arguments: Map<String, Value>,
}

pub struct OverriddenTool {
    inner: Arc<dyn AgentTool>,
    definition: ToolDefinition,
    spec: ToolDefinitionOverride,
}

impl OverriddenTool {
    pub fn new(inner: Arc<dyn AgentTool>, spec: ToolDefinitionOverride) -> Result<Self, String> {
        let mut definition = inner.definition();
        if let Some(name) = &spec.rename {
            definition.name.clone_from(name);
        }
        match &spec.description {
            Some(DescriptionOverride::Replace(text)) => definition.description.clone_from(text),
            Some(DescriptionOverride::Append(text)) if !text.is_empty() => {
                if !definition.description.is_empty() {
                    definition.description.push('\n');
                }
                definition.description.push_str(text);
            }
            _ => {}
        }
        if let Some(required) = definition
            .parameters
            .get_mut("required")
            .and_then(Value::as_array_mut)
        {
            for name in &spec.hidden_parameters {
                if required.iter().any(|field| field.as_str() == Some(name))
                    && !spec.default_arguments.contains_key(name)
                {
                    return Err(format!(
                        "Set a default before hiding required parameter '{name}'"
                    ));
                }
            }
            required.retain(|field| {
                !field
                    .as_str()
                    .is_some_and(|name| spec.hidden_parameters.iter().any(|hidden| hidden == name))
            });
        }
        if let Some(properties) = definition
            .parameters
            .get_mut("properties")
            .and_then(Value::as_object_mut)
        {
            for name in &spec.hidden_parameters {
                properties.shift_remove(name);
            }
            for (name, description) in &spec.parameter_descriptions {
                if let Some(property) = properties.get_mut(name).and_then(Value::as_object_mut) {
                    property.insert("description".into(), Value::String(description.clone()));
                }
            }
        }

        Ok(Self {
            inner,
            definition,
            spec,
        })
    }
}

impl AgentTool for OverriddenTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        self.inner.execution_mode()
    }

    fn execute(
        &self,
        tool_call_id: String,
        mut args: Value,
        cancel: CancelToken,
        on_update: ToolProgressCallback,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        if let Some(args) = args.as_object_mut() {
            for name in &self.spec.hidden_parameters {
                args.shift_remove(name);
            }
            for (name, value) in &self.spec.default_arguments {
                args.entry(name.clone()).or_insert_with(|| value.clone());
            }
        }
        self.inner.execute(tool_call_id, args, cancel, on_update)
    }
}

pub fn wrap_tool_with_override(
    tool: Arc<dyn AgentTool>,
    spec: ToolDefinitionOverride,
) -> Result<Arc<dyn AgentTool>, String> {
    Ok(Arc::new(OverriddenTool::new(tool, spec)?))
}
