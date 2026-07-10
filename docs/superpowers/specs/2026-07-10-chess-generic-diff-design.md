# Chess Generic Diff Design

## 1. 目标与范围

让 `ChessView` 只描述 agent 需要看到的当前状态，并由通用 `AgentView` 字段 diff 生成增量 XML。迁移后不再维护 chess-specific update view、集合比较 helper 或独立的 `render_update_since` 规则。

本次不新增 diff 语法，也不改变领域层的 `ChessBoardView` / `ChessRankView`。唯一的数据形状调整发生在 agent-facing view：棋盘格从按 rank 嵌套改为扁平 keyed vector。

## 2. Agent-facing 数据形状

`ChessView` 保留五个语义字段，但 `board_squares` 直接持有 `Vec<ChessSquarePromptView>`：

```rust
#[derive(AgentView)]
#[agent_view(kind = "prompt_board")]
pub struct ChessView {
    #[view(diff(replace))]
    board_state: ChessBoardStatePromptView,

    #[view(diff(key = "id"))]
    board_squares: Vec<ChessSquarePromptView>,

    #[view(diff(set))]
    legal_moves: Vec<ChessMovePromptView>,

    #[view(name = "move_history", diff(seq))]
    move_history_view: Vec<ChessMovePromptView>,

    #[view(diff(replace))]
    engine: ChessEnginePromptView,
}
```

full render 中 `<board_squares>` 的直接子节点是 64 个 `<square>`。每个 square 自带 `id`、`file`、`rank`，因此 `<rank>` 只提供视觉分组、没有额外语义，可以从 prompt tree 中删除。

## 3. 字段 diff 策略与输出

`board_state` 和 `engine` 是需要整体重读的小对象，使用 `replace`。`legal_moves` 的顺序没有语义，使用 `set`。`move_history` 是只在尾部增长或回退的有序记录，使用 `seq`。`board_squares` 使用 square 的 `id` 做 keyed diff。

例如 `e2e4` 后，棋盘格部分由通用算法输出：

```xml
<board_squares rendering_mode="delta">
  <update>
    <square id="e2" file="e" rank="2">.</square>
  </update>
  <update>
    <square id="e4" file="e" rank="4">P</square>
  </update>
</board_squares>
```

根节点统一使用 `rendering_mode="delta"`，集合操作统一使用 `insert`、`remove`、`update`、`replace`。不再保留 chess-specific 的 `render_mode="update"`、`added` 或 `removed` 词汇。

## 4. 收集与渲染流程

领域快照仍先生成 `ChessBoardView`，以复用 ASCII board 与现有领域访问逻辑。`ChessView::collect` 在 agent-facing 边界把 `board.ranks[*].squares[*]` 展平并收集成 `ChessSquarePromptView`。

full 路径继续通过 blanket `PromptRenderable` 实现调用 `render_agent_view_xml`。delta 路径统一通过 blanket `ContextView::render_delta` 调用 `render_agent_view_diff_xml`。example 与 CLI 不再调用 chess 自有渲染入口；没有变化时 `render_delta` 返回 `None`，调用方省略该 delta block。

## 5. 清理范围与测试

删除只为旧 update XML 服务的 `Chess*UpdateView`、`Chess*ReplaceView`、`list_added`、`list_removed`、`ordered_list_delta`、`changed_squares` 与 `render_update_since`。领域模型、走棋规则、engine 流程和 turn prompt 不变。

测试覆盖三层行为：full chess view 直接渲染 square；一次 `e2e4` 产生字段级 generic delta；CLI/示例消费 `ContextView::render_delta`。通用 diff 单元测试继续约束 keyed、set、seq 与 replace 的独立语义，chess 集成测试只验证这些模式组合后的真实输出。
