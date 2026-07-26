import assert from 'node:assert/strict';
import {
  approvalOptionLabel,
  type ApprovalTranslate,
} from './executorApprovalPresentation';

const translate: ApprovalTranslate = (key, fallback, replacements = {}) =>
  Object.entries(replacements).reduce(
    (value, [name, replacement]) =>
      value.replaceAll(`{${name}}`, String(replacement)),
    fallback || key,
  );

assert.equal(
  approvalOptionLabel(
    { option_id: 'once', kind: 'allow_once', label: 'Proceed' },
    translate,
  ),
  'Allow once',
);
assert.equal(
  approvalOptionLabel(
    { option_id: 'always', kind: 'allow_always', label: 'Proceed always' },
    translate,
  ),
  'Always allow',
);
assert.equal(
  approvalOptionLabel(
    { option_id: 'custom', kind: 'other', label: 'Ask agent' },
    translate,
  ),
  'Ask agent',
);

console.log('executor approval presentation tests passed');
