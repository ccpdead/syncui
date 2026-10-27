import { useId, useRef, useState } from "react";

const PRESET_RULES = [
  ".git",
  "node_modules",
  ".venv",
  "__pycache__",
  "target",
  "dist",
  "build",
  "install",
  "log",
  ".DS_Store",
];

// These simple-name variants already have the same meaning in the engine:
// build, build/, and **/build. Display them as a single checkbox.
function ruleKey(rule: string): string {
  const trimmed = rule.trim().replace(/\/+$/, "");
  return /^\*\*\/[^/]+$/.test(trimmed) ? trimmed.slice(3) : trimmed;
}

function uniqueRules(rules: string[]): string[] {
  const byKey = new Map<string, string>();
  for (const rule of rules) {
    const trimmed = rule.trim();
    const key = ruleKey(trimmed);
    if (key && !byKey.has(key)) byKey.set(key, trimmed);
  }
  return [...byKey.values()];
}

export default function IgnoreRules({
  value,
  onChange,
  disabled,
}: {
  value: string[];
  onChange: (rules: string[]) => void;
  disabled: boolean;
}) {
  const customId = useId();
  const dialogRef = useRef<HTMLDialogElement>(null);
  const [draft, setDraft] = useState("");
  const [draftRules, setDraftRules] = useState<string[]>([]);
  // Keep custom choices visible after unchecking them during this session.
  const [customChoices, setCustomChoices] = useState<string[]>([]);
  const choices = uniqueRules([...PRESET_RULES, ...customChoices, ...value, ...draftRules]);
  const selected = new Set(draftRules.map(ruleKey));
  const appliedCount = new Set(value.map(ruleKey)).size;
  const additions = uniqueRules(draft.split(/[,，]/));

  const openSelector = () => {
    setDraftRules([...value]);
    setDraft("");
    setCustomChoices((prev) => uniqueRules([...prev, ...value]));
    dialogRef.current?.showModal();
  };

  const toggle = (rule: string, checked: boolean) => {
    setDraftRules((prev) =>
      checked
        ? uniqueRules([...prev, rule])
        : prev.filter((item) => ruleKey(item) !== ruleKey(rule))
    );
  };

  const addRules = () => {
    if (disabled || additions.length === 0) return;
    setCustomChoices((prev) => uniqueRules([...prev, ...additions]));
    setDraftRules((prev) => uniqueRules([...prev, ...additions]));
    setDraft("");
  };

  return (
    <>
      <div className="opt ignore-picker">
        <span>忽略</span>
        <button
          type="button"
          className="btn ignore-trigger"
          disabled={disabled}
          aria-haspopup="dialog"
          onClick={openSelector}
          title={value.length ? value.join(", ") : "未启用忽略规则"}
        >
          选择规则 <span className="ignore-count">已选 {appliedCount} 项</span>
          <span aria-hidden="true">▾</span>
        </button>
      </div>
      <dialog
        ref={dialogRef}
        className="ignore-dialog"
        aria-labelledby={`${customId}-title`}
        aria-describedby={`${customId}-description`}
      >
        <form
          className="ignore-dialog-form"
          onSubmit={(event) => {
            event.preventDefault();
            if (disabled) return;
            if (draftRules.join(", ") !== value.join(", ")) onChange(draftRules);
            dialogRef.current?.close();
          }}
        >
          <header className="ignore-dialog-header">
            <div>
              <h2 id={`${customId}-title`}>选择忽略规则</h2>
              <p id={`${customId}-description`}>勾选需要忽略的文件或目录，点击“应用规则”后生效。</p>
            </div>
            <button
              type="button"
              className="ignore-close"
              aria-label="关闭规则选择器"
              onClick={() => dialogRef.current?.close()}
            >
              ×
            </button>
          </header>
          <div className="ignore-dialog-body">
            <fieldset className="ignore-rules" disabled={disabled}>
              <legend>可用规则</legend>
              <div className="ignore-selection-actions">
                <button type="button" className="link" onClick={() => setDraftRules(choices)}>全选</button>
                <button type="button" className="link" onClick={() => setDraftRules([])}>清空</button>
              </div>
              <div className="ignore-options">
                {choices.map((rule) => {
                  const checked = selected.has(ruleKey(rule));
                  return (
                    <label
                      key={ruleKey(rule)}
                      className={`ignore-rule${checked ? " checked" : ""}`}
                      title={`勾选后忽略：${rule}`}
                    >
                      <input
                        type="checkbox"
                        checked={checked}
                        onChange={(event) => toggle(rule, event.target.checked)}
                      />
                      <span>{rule}</span>
                    </label>
                  );
                })}
              </div>
              <div className="ignore-custom">
                <label htmlFor={customId}>自定义规则</label>
                <div className="ignore-custom-entry">
                  <input
                    id={customId}
                    type="text"
                    value={draft}
                    onChange={(event) => setDraft(event.target.value)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter" && !event.nativeEvent.isComposing) {
                        event.preventDefault();
                        addRules();
                      }
                    }}
                    placeholder="例如 *.bkp 或 src/cache"
                    aria-describedby={`${customId}-hint`}
                  />
                  <button type="button" className="btn" disabled={additions.length === 0} onClick={addRules}>
                    添加
                  </button>
                </div>
                <span id={`${customId}-hint`} className="ignore-hint">多个规则用逗号分隔，添加后也可取消勾选。</span>
              </div>
            </fieldset>
          </div>
          <footer className="ignore-dialog-footer">
            <span className="ignore-count">已选择 {selected.size} 项规则</span>
            <button type="button" className="btn" onClick={() => dialogRef.current?.close()}>取消</button>
            <button type="submit" className="btn primary" disabled={disabled}>应用规则</button>
          </footer>
        </form>
      </dialog>
    </>
  );
}
