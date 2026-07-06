-- Migration: Add GitHub Copilot CLI summary provider configuration

-- This column stores: {binaryPath, model, githubToken}
ALTER TABLE settings ADD COLUMN copilotCliConfig TEXT;
