use agentview::component::{authoring::NativeTool, prelude::*, ComponentHost};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, JsonSchema)]
struct Adjustment {
    amount: i32,
    labels: Vec<String>,
}

#[derive(Debug, PartialEq, Serialize)]
struct Sum {
    value: i32,
}

/// Adds a value and an optional structured adjustment.
#[tool]
fn add(
    /// The base value.
    value: i32,
    /// An optional adjustment with nested JSON-schema fields.
    adjustment: Option<Adjustment>,
) -> Result<Sum, ToolError> {
    let amount = adjustment
        .map(|adjustment| adjustment.amount + adjustment.labels.len() as i32 - 1)
        .unwrap_or_default();
    Ok(Sum {
        value: value + amount,
    })
}

#[tool(description = "Formats an owned value asynchronously.")]
async fn format_value(value: String) -> Result<String, ToolError> {
    Ok(format!("value:{value}"))
}

#[tool]
fn r#match(value: String) -> Result<String, ToolError> {
    Ok(value)
}

#[component]
fn mounted_tool() -> Component {
    NativeToolCall::new(add)
}

#[tokio::test]
async fn tool_macro_generates_owned_args_schema_and_sync_handler() {
    let output = add
        .call(AddArgs {
            value: 4,
            adjustment: Some(Adjustment {
                amount: 3,
                labels: vec!["bonus".to_owned()],
            }),
        })
        .await
        .unwrap();
    assert_eq!(output, Sum { value: 7 });
    assert_eq!(<AddTool as NativeTool>::NAME, "add");
    assert_eq!(
        <AddTool as NativeTool>::DESCRIPTION,
        "Adds a value and an optional structured adjustment."
    );

    let schema = serde_json::to_value(schemars::schema_for!(AddArgs)).unwrap();
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(
        schema["properties"]["value"]["description"],
        "The base value."
    );
    assert!(schema["properties"]["adjustment"]
        .to_string()
        .contains("null"));

    let _: Component = mounted_tool();
    let mut host = ComponentHost::new_root(|_| mounted_tool(), ());
    let projection = host.render().unwrap().projection().clone();
    assert_eq!(
        projection
            .native_tools()
            .iter()
            .map(|tool| tool.name())
            .collect::<Vec<_>>(),
        ["add"],
        "the mounted catalog uses the annotated function name"
    );
}

#[tokio::test]
async fn tool_macro_supports_async_handlers_and_error_values() {
    let output = format_value
        .call(FormatValueArgs {
            value: "hello".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(output, "value:hello");
    assert_eq!(
        <FormatValueTool as NativeTool>::DESCRIPTION,
        "Formats an owned value asynchronously."
    );
    assert_eq!(
        ToolError::from("missing value").to_string(),
        "missing value"
    );

    let output = r#match
        .call(MatchArgs {
            value: "keyword".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(output, "keyword");
    assert_eq!(<MatchTool as NativeTool>::NAME, "match");
}
