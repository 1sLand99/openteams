// Run with:
//   pnpm exec tsx src/components/NotificationToast.test.tsx

import assert from 'node:assert/strict';
import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import {
  NotificationToast,
  type NotificationToastTone,
} from './NotificationToast';

const expectedIconClass: Record<NotificationToastTone, string> = {
  info: 'lucide-info',
  success: 'lucide-circle-check',
  warning: 'lucide-triangle-alert',
  error: 'lucide-circle-x',
};

for (const [tone, iconClass] of Object.entries(expectedIconClass) as Array<
  [NotificationToastTone, string]
>) {
  const markup = renderToStaticMarkup(
    <NotificationToast message={`${tone} message`} tone={tone} />,
  );

  assert.match(markup, new RegExp(`class="[^"]*${iconClass}`));
}

const warningMarkup = renderToStaticMarkup(
  <NotificationToast message="Warning message" tone="warning" />,
);

assert.match(warningMarkup, /text-\[var\(--notification-warning\)\]/);
assert.match(warningMarkup, /bg-\[var\(--notification-warning-bg\)\]/);
assert.match(warningMarkup, /border-\[var\(--notification-warning-border\)\]/);
assert.doesNotMatch(warningMarkup, /amber/);

console.log('Notification toast icons: PASS');
