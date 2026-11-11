# Meeting Summary Templates

This directory contains template definitions for meeting summary generation.

## Available Templates

Every `*.json` file in this directory is bundled with the app and appears in the template picker; no code change is needed to add one. Keep the set small: each template covers one kind of meeting, and `standard_meeting` is the fallback for anything else.

| ID | Name | Use for |
| --- | --- | --- |
| `briefing_training` | Briefing & Training | One-to-many sessions: town halls, release presentations, trainings and knowledge transfers, and internal team meetings |
| `client_account` | Client Account & Sales | Internal account-team reviews of one client (CET), executive engagement debriefs, and sales or pre-sales opportunity calls |
| `client_workshop` | Client Workshop | Client design and requirements workshops, kickoffs, scoping and estimation reviews, and reviews of workshop minutes or open points |
| `delivery_sync` | Delivery Sync | Project delivery calls: stream dailies, case and defect reviews, Delta release and go-live readiness, production issues and technical investigations |
| `people_staffing` | People & Staffing | 1:1s, staffing and pipeline reviews, year-end calibration and talent discussions, headcount and hiring |
| `standard_meeting` | Standard Meeting Notes | A standard template for general meetings, focusing on key outcomes and actions |

Retired IDs (`daily_standup`, `project_sync`, `sales_marketing_client_call`, `retrospective`, `psychatric_session`) still load: `defaults::replacement_for_retired_template` maps each to its replacement, unless a custom template with that ID exists.

Section instructions must not end with a period: the prompt builder appends one. `test_bundled_template_files_are_valid` checks this and validates every file.

## Template Structure

Each template JSON file follows this schema:

```json
{
  "name": "Template Name",
  "description": "Brief description of the template's purpose",
  "sections": [
    {
      "title": "Section Title",
      "instruction": "Instructions for the LLM on what to extract/include",
      "format": "paragraph|list|string",
      "item_format": "Optional: Markdown table format for list items"
    }
  ]
}
```

## Custom Templates

Users can add custom templates to the application data directory:

- **macOS**: `~/Library/Application Support/Meetily/templates/`
- **Windows**: `%APPDATA%\Meetily\templates\`
- **Linux**: `~/.config/Meetily/templates/`

Custom templates override built-in templates with the same filename.

## Template Fields

### Root Level
- `name` (required): Display name for the template
- `description` (required): Brief explanation of the template's use case
- `sections` (required): Array of section definitions

### Section Object
- `title` (required): Section heading text
- `instruction` (required): LLM guidance for this section
- `format` (required): One of `"paragraph"`, `"list"`, or `"string"`
- `item_format` (optional): Markdown formatting hint for list items (e.g., table structure)
- `example_item_format` (optional): Alternative formatting hint

## Usage in Code

Templates are loaded using the `templates` module:

```rust
use crate::summary::templates;

// Get a specific template
let template = templates::get_template("standard_meeting")?;

// List available templates
let available = templates::list_templates();

// Validate custom template JSON
let custom_json = std::fs::read_to_string("custom.json")?;
let validated = templates::validate_template(&custom_json)?;
```
