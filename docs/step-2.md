# Step 2 notes: templates, automations, scripts

What each part of roadmap step 2 built and how it was checked against HA. The checklist is in
[roadmap.md](roadmap.md#2-automations-scripts-templates); this page keeps the detail behind each
checked item, as it was written when the item was done.

Same method as steps 0 and 1: record real HA first, build against the recording, add the scenario
to the CI replay.

## Record

(Done: `compat/capture-mqtt.sh automations`, `compat/fixtures/automations/`.) A scenario: a few automations and scripts in YAML
(state, time and MQTT triggers; conditions; service calls, delays, `choose`), the frontend's
automation and script pages, and template rendering (`render_template` with `states()`).

## Templates

(Done: `crates/lean-home/src/template.rs`.) HA's globals and filters on
minijinja: `states` (call, `states.domain.x`, iteration), `is_state`, `is_state_attr`,
`state_attr`, `has_value`, `float`/`int`/`bool` with defaults, `is_number`, `iif`,
`is_defined`, `round`, `now()`/`utcnow()`/`as_timestamp`, `to_json`/`from_json`; Python-style
output (`True`, `None`, `['a']`) and HA's result parsing. MQTT value and command templates
use it, and `render_template` re-renders on the entities, domains or minute it read. Cases
come from HA's source and the `automations` recording (the live template's 7 results match in
order). The ha-src checkout has no `tests/`, so HA's own template tests are still to mine.
Measured on the dev PC: stripped binary +144 KiB (6.32 → 6.47 MB); process RSS (a quick
check, not the cgroup method of the [measurement rules](architecture.md#measurement-rules)) unchanged at
idle (~6.6 MB) and ~0.3 MB more with 200 live `render_template` subscriptions.

## Config

(Done: `crates/lean-home/src/config.rs`.) `configuration.yaml` as HA's loader
(`annotatedyaml` on PyYAML) reads it: YAML 1.1 scalars (unquoted `on` is `true`, `3:00:00`
is 10800, `1e5` a string), anchors, `<<` merge keys, duplicate keys (last wins), and the
tags `!include`, `!include_dir_list`/`_merge_list`/`_named`/`_merge_named`, `!secret`
(nearest `secrets.yaml` up to the config dir), `!env_var`. Automations come from every
`automation`/`automation <label>` key and scripts from `script`/`script <label>`; they are
collected here and run in step 4. The **core config** (defaults, then
`.storage/core.config`, then `homeassistant:` keys) now fills `get_config`, including
`config_source`, the units and the **time zone**: templates' `now()` is local time in it, and
naive times in `as_timestamp` are read in it. Time zones come from the system zoneinfo, like
HA's Python `zoneinfo`; UTC needs none. Checked against HA: the `automations` scenario's
config parses to exactly what HA's loader makes of it
(`compat/fixtures/automations/configuration.json`), the scalar cases are PyYAML's own
results, and DST gaps and overlaps match Python's `zoneinfo`. Measured on the dev PC under the
[measurement rules](architecture.md#measurement-rules) (`bench/bench.sh`, 1 CPU, 1 GiB): anon memory
unchanged at 0.6 MiB idle, peak 1.2 MiB; stripped binary +237 KiB (6.47 → 6.71 MB).
`chrono-tz` was tried first and dropped: its embedded time-zone database cost +2.1 MB of
binary and +2 MB RSS.

## Automations and scripts

Start with two checks on the template engine from 2.2, since
conditions and action data now depend on it, so a template bug is fixed in the step it would
break:

## HA's template tests

(Done.) Instead of porting asserts by hand,
`compat/template-cases.py` takes every literal template from HA's
`tests/helpers/template/` (with the states each test sets) and has HA itself render it
(`compat/fixtures/templates/cases.jsonl`: text, parsed result, error, listeners). A test
replays them through lean-home: of 611 cases, the 195 that use only implemented helpers
all match HA, except 2 documented gaps (a listener HA adds on `state == "on"`
comparisons, which can't change the result). The other 416 use HA helpers not built yet
(see the gaps ([roadmap.md](roadmap.md#gaps))). It found and fixed: keyword arguments (`float(x, default=1)`,
`int(x, base=16)`), an explicit `none` default, `states | count` listening to every
change instead of only additions and removals, extra listeners from `states.x.y`,
entity and domain name validation, `to_json` (datetimes error; `ensure_ascii` is Python's
`json`), static templates' results staying unparsed, HA's 256 KiB output limit, dict
order (minijinja `preserve_order`), Jinja's `urlencode` and HA's `random`.

## Template errors

(Done.) The `automations` scenario now records five failing or
warning templates. HA sends each log and error event both before and after the result
(setup, then the first render), and an undefined variable adds an `'x' is undefined`
line (`WARNING` when rendered, `ERROR` before an `UndefinedError`). lean-home now does the
same, checked message for message against the recording (`ws.rs` tests).

## Conditions, actions and scripts

(Done: `condition.rs`, `script.rs`, `trace.rs`.)
Conditions `state`, `numeric_state`, `template`, `and`/`or`/`not` and their shorthands;
actions: service calls (templated `action`, `data`, `target`), `delay`, `choose`, `if`,
`condition`, `variables`, `stop`, with `enabled` and `continue_on_error`; script modes
`single`/`restart`/`queued`/`parallel` with `max` and HA's refusals; the `script.*`
entities (`last_triggered`, `mode`, `current`, `max`, `last_action`, HA's 5 s context
reuse) and services (`script.<key>` waits, `turn_on` with `variables` doesn't, `turn_off`,
`toggle`, and `get_services` with HA's field and selector defaults). Runs are traced as
they execute, HA's way (elements by config path, changed variables, results, errors,
`child_id` links to called scripts, the last 5 runs per script), and served by
`trace/list` and `trace/get`. A script call over the WebSocket is answered when the run
ends while the connection keeps serving, as in HA. Checked against the recording (which
now opens the script's trace page): replaying `script.blink`'s two runs gives HA's state
changes, `trace/get` and `trace/list` exactly. Measured (`bench/bench.sh`): anon memory
idle unchanged at 0.6 MiB; 0.85 MiB after 20 runs with 5 traces stored and a WebSocket
session open; stripped binary +334 KiB (6.79 → 7.14 MB).

## Triggers and automations

(Done: `automation.rs`.) State triggers (`from`/`to`,
`not_from`/`not_to`, `attribute`, `for:` with templates), MQTT triggers (`payload`,
`value_template`, `qos`; the topic is subscribed with the hub's own), time triggers at fixed
times with `weekday` (re-checked against the wall clock every minute, so a board that boots
in 1970 and then gets NTP doesn't fire at the wrong time), trigger `id`/`alias`/`enabled`/
`variables`; the `trigger` condition and the `state` condition's `for:`. An automation's
entity is a `ScriptEntity` in the `automation` domain (`id`, `last_triggered`, `mode`,
`current`, `max`; `initial_state`; unavailable for a config it can't run) and its actions
run in `script.rs` with a context whose `parent_id` is the trigger's. Runs are traced HA's
way (`trigger/<idx>` with the trigger variables, `condition/<i>`, `action/…`,
`failed_conditions`, the trigger description in `trace/list`). Services `turn_on`,
`turn_off` (`stop_actions`), `toggle` and `trigger` (`skip_condition`, `variables`; answered
when the run ends). Checked against the recording: the fan automation's state changes,
`trace/list` and `trace/get` exactly, the button's and the nightly one's (03:00 on a paused
clock), and the `automations` scenario joined the CI replay (21 state changes, 10 MQTT
messages as recorded). Measured on the dev PC (single run, `RssAnon` rather than
`bench/bench.sh`'s cgroup): stripped binary +183 KiB (7.14 → 7.32 MB); idle anon memory
+36 KiB (984 → 1020 KiB) with the scenario's 3 automations.

## The editors, read-only

(Done: `http.rs`, `ws.rs`, `registry.rs`.) The automation and
script editors open and show a config as HA does: `GET /api/config/automation/config/<id>`
and `/api/config/script/config/<key>` (admins only, read from `automations.yaml` and
`scripts.yaml` on each request, as HA's view does), `config/entity_registry/list`,
`config_entries/subscribe` (the stored entries; lean-home's own loaded, the rest
`not_loaded`), `manifest/list` (lean-home's integrations, from `manifests.json`),
`subscribe_condition` (the editor's live condition results), and the MQTT trigger in
`trigger_platforms/subscribe`. Blueprints, categories and icons answer empty. The
`automations` scenario matches with no tolerances left in `compare.py`: both config
requests, and the condition results HA sent. Measured on the dev PC (single run, `RssAnon`):
stripped binary +83 KiB (7.32 → 7.40 MB); idle anon memory 1020 → 1076 KiB, within a
single run's noise (the manifests load only when asked for).

## The editors, saving

(Done: `editor.rs`, `yaml_dump.rs`; `reload` in `automation.rs`
and `script.rs`.) Recorded first: the scenario now ends with saves from the editors (the
nightly automation moved to 04:00, a new automation added and fired, a new script added and
run, both deleted, one read back) and keeps the files HA wrote
(`compat/fixtures/automations/*.saved.yaml`). `POST`/`DELETE` on
`/api/config/{automation,script}/config/<key>` change the file the way HA's views do (`id`
first and the editor's key order, an `id` for entries without one, `Key malformed` for a
script key that isn't a slug, 400 for a missing `triggers`/`actions`/`sequence`), write it
atomically (temp file, fsync, rename, directory fsync) and keep the previous one as
`<file>.bak`, then reload: one automation by `id`, every changed script. Reloading keeps
unchanged ones running, shows a replaced or dropped entity as HA does (`unavailable`,
`restored: true`) before the new one, which keeps on/off and `last_triggered`; a delete
removes the entity. `automation.reload` (all, or one `id`) and `script.reload` work as
services too. The files are written with a YAML emitter that follows libyaml's (HA's
`CSafeDumper`): checked against 111 values HA's own dumper wrote
(`compat/fixtures/yaml-dump/`), and the scenario's two files come out byte for byte.
`compare.py` now also follows entities added and removed after the page loaded, and
compares the saved files. Measured on the dev PC (single run, `RssAnon`): stripped binary
+142 KiB (7.40 → 7.55 MB); idle anon memory 1076 → 1092 KiB, single-run noise.

## Packages

(Done: `merge_packages` in `config.rs`.) `homeassistant: packages:` is merged
as HA's `merge_packages_config` does: `automation` entries appended (falsy ones dropped, a
single mapping made a list, labelled keys like `automation extra` kept), `script` keys
merged recursively, a package whose name isn't a slug skipped, a dict domain given a list
refused, and HA's quirk kept (a later key's result hides a duplicate key it found, so
`script.blink` in a package merges into the main one without an error). Recorded first:
`compat/packages/` loaded by HA's own `async_hass_config_yaml` in the image
(`compat/fixtures/packages/merged.json`, the merged keys and HA's errors); lean-home gives
the same keys in the same order and the same messages. Automations and scripts in packages
load and reload like any other; the editors don't change them (HA's only edit
`automations.yaml`/`scripts.yaml`). Measured like for like against `main` (release, stripped):
7.55 → 7.48 MB, the compiler laying code out differently rather than a saving.

## Restore state

(Done: `restore.rs`; not in the plan above, picked from the gaps ([roadmap.md](roadmap.md#gaps)),
where it was wrongly tied to the recorder: HA keeps it in `.storage/core.restore_state`, not
the database.) A restart used to turn every automation back on; now an automation keeps
on/off and `last_triggered`, and a script `last_triggered` (`initial_state` still wins), read
from HA's file at startup. lean-home writes the file as HA does: at startup, every 15
minutes and at shutdown, with its automations and scripts as current entries; entries for
entities this run doesn't have (HA's other integrations, entities removed by a reload or
the editor) are kept for 7 days, so a config dir shared with HA loses nothing. Recorded
first: HA's file after the `automations` scenario
(`compat/fixtures/automations/core.restore_state.json`, now captured with the other
`.storage` files); lean-home restores from it, and after the replay its own file has the
same automation and script entries, the two the editors deleted included (`compare.py`).
Measured (release, stripped): 7.48 → 7.58 MB.
