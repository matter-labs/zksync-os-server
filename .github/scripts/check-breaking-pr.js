module.exports = function ({ github, context, core }) {
    const pr = context.payload.pull_request;

    if (!pr) {
        core.info('No pull_request found in context; skipping.');
        return;
    }

    const title = pr.title || '';
    const body = pr.body || '';

    // --- Detect semver-breaking conventional commit ("!:")
    const isBreaking = /!:/.test(title);

    if (!isBreaking) {
        core.info('PR title is not semver-breaking (no "!:"), skipping check.');
        return;
    }

    core.info(`PR "${title}" is marked as breaking; checking required sections...`);

    // --- Helpers ---
    function getSectionLines(markdown, headingText) {
        const lines = markdown.split('\n');
        const headingLower = headingText.trim().toLowerCase();

        // Find heading (exact, case-insensitive)
        const startIdx = lines.findIndex(
            (l) => l.trim().toLowerCase() === headingLower
        );

        if (startIdx === -1) return null;

        // Collect lines until next heading or EOF
        const result = [];
        for (let i = startIdx + 1; i < lines.length; i++) {
            if (/^##\s+/.test(lines[i])) break;
            result.push(lines[i]);
        }
        return result;
    }

    function hasNonCommentContent(lines) {
        if (!lines || lines.length === 0) return false;

        let inBlockComment = false;

        for (const raw of lines) {
            const line = raw.trim();
            if (!line) continue;

            // HTML block comment start
            if (inBlockComment) {
                if (line.includes('-->')) inBlockComment = false;
                continue;
            }

            if (line.startsWith('<!--')) {
                if (!line.includes('-->')) {
                    inBlockComment = true;
                }

                // If the line is *purely* a comment, ignore it
                if (line.replace(/<!--|-->/g, '').trim() === '') {
                    continue;
                }
                // If comment + content → treat as content
            }

            return true;
        }

        return false;
    }

    // --- Validate Sections ---
    const breakingLines = getSectionLines(body, '## Breaking Changes');
    const rolloutLines = getSectionLines(body, '## Rollout Instructions');

    const errors = [];

    if (!breakingLines) {
        errors.push('Missing "## Breaking Changes" section in PR description.');
    } else if (!hasNonCommentContent(breakingLines)) {
        errors.push('"## Breaking Changes" section is empty or only commented out.');
    }

    if (!rolloutLines) {
        errors.push('Missing "## Rollout Instructions" section in PR description.');
    } else if (!hasNonCommentContent(rolloutLines)) {
        errors.push('"## Rollout Plan" section is empty or only commented out.');
    }

    if (errors.length > 0) {
        core.setOutput('error_message', errors.join('\n'));
        core.setFailed(errors.join('\n'));
    } else {
        core.info('Breaking-change sections are present and non-empty. ✅');
    }
};
