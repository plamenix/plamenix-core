/**
 * @plamenix/plugin-json-cell-renderer — tri-state validator for the
 * Plamenix plugin architecture.
 *
 * Contributes ONE cell renderer at the `cell_renderers` extension
 * point: claims VARCHAR cells whose text content parses as JSON, then
 * renders the parsed value as a formatted `<pre>` block. Pure data
 * contribution — no host imports required (`world = plugin-minimal`),
 * no permissions, no `activate` work beyond the static registration
 * the loader does automatically from the default export's
 * `contributions` field.
 *
 * Demonstrates the M1 plugin contract end-to-end:
 *
 *   - `targets = ["desktop", "web"]` in the manifest → same `.plx`
 *     loads on both editions (verified by the integration test in
 *     `plamenix-ui/src/db/json-cell-renderer-plugin.test.tsx`).
 *   - `world = plugin-minimal` → no host imports linked beyond the
 *     baseline `log` / `host-version` / `edition` / `plugin.activate`.
 *   - Externalised `react` + `react/jsx-runtime` (per `vite.config.ts`)
 *     so the plugin shares the host's React instance — Hooks rules
 *     hold across the boundary.
 *   - Pure JS payload data + Component → the host's `<PluginOutlet>`
 *     consumer (wired in I3.4 inside `ResultTable.CellContent`)
 *     dispatches automatically. No host-side change needed to make
 *     this plugin work on top of the M1 shipping shell.
 */

import { useMemo } from 'react';
import type {
  CellRendererContext,
  CellRendererPayload,
  PluginUiModule,
} from '@plamenix/ui';

/** Cheap heuristic: skip text cells whose first non-whitespace
 *  character is not a brace or bracket. Avoids `JSON.parse` cost on
 *  every text cell across a virtualised grid. */
const JSON_LEAD = /^\s*[{[]/;

function matchesJsonCell(ctx: CellRendererContext): boolean {
  if (ctx.cell.type !== 'text') return false;
  return JSON_LEAD.test(ctx.cell.value);
}

function JsonTreeCell({ ctx }: { ctx: CellRendererContext }) {
  const formatted = useMemo(() => {
    if (ctx.cell.type !== 'text') return null;
    try {
      const parsed = JSON.parse(ctx.cell.value) as unknown;
      return JSON.stringify(parsed, null, 2);
    } catch {
      return null;
    }
  }, [ctx.cell]);

  if (formatted === null) {
    // Parse failed despite the lead-char heuristic (e.g. an unbalanced
    // brace). Fall through visually by rendering the raw value so the
    // user does not see a blank cell.
    return ctx.cell.type === 'text' ? <>{ctx.cell.value}</> : null;
  }

  return (
    <pre
      data-plugin="dev.plamenix.json-cell-renderer"
      data-contribution="json-tree"
      style={{
        margin: 0,
        fontFamily: 'ui-monospace, SFMono-Regular, monospace',
        fontSize: '11px',
        whiteSpace: 'pre-wrap',
        wordBreak: 'break-word',
      }}
    >
      {formatted}
    </pre>
  );
}

const jsonRenderer: CellRendererPayload = {
  matches: matchesJsonCell,
  Component: JsonTreeCell,
};

const module: PluginUiModule = {
  contributions: {
    cell_renderers: [
      {
        id: 'json-tree',
        priority: 50,
        payload: jsonRenderer,
      },
    ],
  },
  async activate(api) {
    api.log('info', 'JSON cell renderer ready (' + api.pluginId + ')');
  },
};

export default module;
