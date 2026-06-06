# Teams Meeting Detection Manual QA

Use this checklist on Windows with the new Microsoft Teams app (`ms-teams.exe`).

## Setup

- Confirm Microsoft Teams classic (`Teams.exe`) is not the target app.
- Enable `Meeting Detection` in Meetily preferences.
- Keep `Detect Teams calls`, `Suggest start recording`, and `Suggest stop recording` enabled.
- Confirm Meetily is not already recording before start-prompt scenarios.

## Start Prompt

- Teams closed, Meetily open: no start prompt appears.
- Teams open but idle: no start prompt appears.
- Teams meeting active with remote audio: start prompt appears after about 30 seconds.
- Teams meeting active while the local user is muted but remote audio is present: start prompt appears after about 30 seconds.
- Silent Teams meeting: no prompt or delayed prompt is acceptable for the MVP.
- Clicking `Start Recording` routes through the normal Meetily recording start flow.
- Clicking `Dismiss` suppresses repeated start prompts during the configured cooldown.
- Clicking `Do not ask again` disables future start prompts in preferences.

## Stop Prompt

- While Meetily is recording and Teams audio remains active: no stop prompt appears.
- While Meetily is recording and Teams audio becomes inactive with Teams still open: stop prompt appears after about 60 to 90 seconds.
- Clicking `Stop Recording` routes through the normal Meetily stop and post-processing flow.
- Clicking `Dismiss` suppresses repeated stop prompts during the configured cooldown.
- Clicking `Do not ask again` disables future stop prompts in preferences.

## Background And Diagnostics

- With the Meetily window hidden, prompt handling still works through the mounted root provider.
- Disabling `Meeting Detection` in preferences stops prompt emission.
- Re-enabling `Meeting Detection` starts polling again without restarting the app.
- If Windows audio-session detection fails, `get_meeting_detection_status` reports `last_error` and no audio-confidence prompt is emitted from process presence alone.
