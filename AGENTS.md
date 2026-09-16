# Working on AgentView

Read [HELP.md](HELP.md) before authoring or reviewing Components.
[docs/engine.md](docs/engine.md) is the authoritative runtime contract.

AgentView builds a user interface for the LLM. Interactivity is a primary design
requirement: the model observes, acts, sees the result, and can act again.

- Design the whole interaction. Present current state and discoverable actions,
  including the inputs and constraints needed to use them.
- After an action, expose its actual result, progress, or failure and the updated
  state. Give the model enough information to choose its next action.
- Make feedback part of the application-declared view and arrange for its
  delivery while it is useful. A Signal write or log entry alone does not reach
  the model; the next submitted Frame delivers the updated view.
- Verify the observe -> act -> feedback -> next action flow. Parser acceptance,
  a successful API call, or correct final output alone does not demonstrate an
  interactive experience.
- Error feedback is one part of this design. Skip ordinary invalid model actions,
  retain useful feedback, and continue valid independent actions. Business
  admission and runtime failure/recovery rules still apply.
