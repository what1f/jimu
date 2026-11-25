use jimu_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolProgressCallback};
use jimu_ai::{BoxFuture, CancelToken, ToolDefinition};
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
            .get("required")
            .and_then(Value::as_array)
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
        }
        let omittable = |name: &str| {
            spec.hidden_parameters.iter().any(|hidden| hidden == name)
                || spec.default_arguments.contains_key(name)
        };
        if let Some(root) = definition.parameters.as_object_mut() {
            prune_required_constraints(root, &omittable);
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

fn prune_required_constraints(obj: &mut Map<String, Value>, omittable: &impl Fn(&str) -> bool) {
    if let Some(required) = obj.get_mut("required").and_then(Value::as_array_mut) {
        required.retain(|field| !field.as_str().is_some_and(omittable));
        if required.is_empty() {
            obj.shift_remove("required");
        }
    }
    if let Some(all_of) = obj.get_mut("allOf").and_then(Value::as_array_mut) {
        all_of.retain_mut(|branch| {
            let Some(branch_obj) = branch.as_object_mut() else {
                return true;
            };
            prune_required_constraints(branch_obj, omittable);
            !branch_obj.is_empty()
        });
        if all_of.is_empty() {
            obj.shift_remove("allOf");
        }
    }
    for key in ["oneOf", "anyOf"] {
        let mut satisfied = false;
        if let Some(branches) = obj.get_mut(key).and_then(Value::as_array_mut) {
            for branch in branches.iter_mut() {
                if let Some(branch_obj) = branch.as_object_mut() {
                    let had_only_required =
                        branch_obj.len() == 1 && branch_obj.contains_key("required");
                    prune_required_constraints(branch_obj, omittable);
                    if had_only_required && branch_obj.is_empty() {
                        satisfied = true;
                    }
                }
            }
        }
        if satisfied {
            obj.shift_remove(key);
        }
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
