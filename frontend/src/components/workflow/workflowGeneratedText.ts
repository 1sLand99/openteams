type TranslateFn = (key: string, options?: Record<string, unknown>) => string;

const REVIEW_LOOP_MESSAGE_PATTERN = /Please review loop "([^"]+)"\./g;
const SKIPPED_RETRY_DECISION_MESSAGE =
  'Loop review feedback targets skipped steps. Choose whether to restart them.';
const SKIPPED_RETRY_PROTOCOL_PREFIX = [
  'workflow',
  'loop_skipped_retry_decision',
].join('.');
const SKIPPED_RETRY_DECISION_TOKEN = `${SKIPPED_RETRY_PROTOCOL_PREFIX}.request`;
const SKIPPED_RETRY_CONTEXT_PREFIX = `${SKIPPED_RETRY_PROTOCOL_PREFIX}.context:`;
const SKIPPED_RETRY_CONTEXT_PATTERN =
  /Skipped steps requiring a decision: ([^\n]+)\n\nReview feedback: ([\s\S]+)/g;
const REVIEW_STEP_RESULT_MESSAGE_PATTERNS = [
  /请审核步骤「([^」]+)」的执行结果/g,
  /Please review step "([^"]+)"\./g,
];
const USER_APPROVED_STEP_RESULT = 'User approved the step result.';
const USER_APPROVED_STEP_RESULT_MESSAGES = [
  USER_APPROVED_STEP_RESULT,
  '用户已批准步骤结果。',
  '用戶已批准步驟結果。',
  'ユーザーがステップ結果を承認しました。',
  '사용자가 단계 결과를 승인했습니다.',
  "L'utilisateur a approuvé le résultat de l'étape.",
  'El usuario aprobó el resultado del paso.',
];
const USER_APPROVED_LOOP_RESULT_MESSAGES = [
  'User approved the loop result.',
  '用户已批准循环结果。',
  '用戶已批准循環結果。',
  'ユーザーがループ結果を承認しました。',
  '사용자가 루프 결과를 승인했습니다.',
  "L'utilisateur a approuvé le résultat de la boucle.",
  'El usuario aprobó el resultado del bucle.',
];
const COMPLETED_STATUS_LINE_PATTERN =
  /(^|\r?\n)(?:Status|状态)[ \t]*[:：][ \t]*(?:DONE|COMPLETED)[ \t]*(?=\r?\n|$)/gim;

function replaceExactGeneratedMessages(
  text: string,
  messages: string[],
  replacement: string
): string {
  return messages.reduce(
    (current, message) => current.replaceAll(message, replacement),
    text
  );
}

export function localizeWorkflowGeneratedText(
  text: string,
  t: TranslateFn
): string {
  let localized = text.replace(
    REVIEW_LOOP_MESSAGE_PATTERN,
    (_match, loopKey: string) =>
      t('workflow.generatedText.reviewLoop', {
        loopKey,
        defaultValue: `Please review loop "${loopKey}".`,
      })
  );

  localized = localized.replace(
    SKIPPED_RETRY_DECISION_MESSAGE,
    t('workflow.generatedText.skippedRetryDecision', {
      defaultValue: SKIPPED_RETRY_DECISION_MESSAGE,
    })
  );
  localized = localized.replace(
    SKIPPED_RETRY_DECISION_TOKEN,
    t('workflow.generatedText.skippedRetryDecision', {
      defaultValue: SKIPPED_RETRY_DECISION_MESSAGE,
    })
  );
  if (localized.startsWith(SKIPPED_RETRY_CONTEXT_PREFIX)) {
    try {
      const payload = JSON.parse(
        localized.slice(SKIPPED_RETRY_CONTEXT_PREFIX.length)
      ) as {
        step_titles?: string;
        feedback?: string;
        keep_effect?: string;
      };
      const keepEffect =
        payload.keep_effect === 'waive_skipped_scope_and_complete_loop'
          ? t('workflow.generatedText.skippedRetryKeepComplete', {
              defaultValue:
                'Keep skipped: waive these nodes and complete the loop.',
            })
          : t('workflow.generatedText.skippedRetryKeepRetry', {
              defaultValue:
                'Keep skipped: waive these nodes and retry only the remaining targets.',
            });
      return t('workflow.generatedText.skippedRetryContextDetailed', {
        stepTitles: payload.step_titles ?? '',
        feedback: payload.feedback ?? '',
        keepEffect,
        defaultValue: `Skipped steps: ${payload.step_titles ?? ''}\nReview feedback: ${payload.feedback ?? ''}\nRestart: rerun these nodes, then review the loop again.\n${keepEffect}`,
      });
    } catch {
      return localized;
    }
  }
  localized = localized.replace(
    `${SKIPPED_RETRY_PROTOCOL_PREFIX}.result.restart_skipped`,
    t('workflow.generatedText.skippedRetryResultRestarted', {
      defaultValue: 'Restart skipped nodes and review the loop again.',
    })
  );
  localized = localized.replace(
    `${SKIPPED_RETRY_PROTOCOL_PREFIX}.result.keep_skipped`,
    t('workflow.generatedText.skippedRetryResultKept', {
      defaultValue: 'Keep skipped nodes and apply the user waiver.',
    })
  );
  localized = localized.replace(
    SKIPPED_RETRY_CONTEXT_PATTERN,
    (_match, stepTitles: string, feedback: string) =>
      t('workflow.generatedText.skippedRetryContext', {
        stepTitles,
        feedback,
        defaultValue: `Skipped steps requiring a decision: ${stepTitles}\n\nReview feedback: ${feedback}`,
      })
  );

  for (const pattern of REVIEW_STEP_RESULT_MESSAGE_PATTERNS) {
    localized = localized.replace(pattern, (_match, stepTitle: string) =>
      t('workflow.generatedText.reviewStepResult', {
        stepTitle,
        defaultValue: `Please review the execution result for step "${stepTitle}".`,
      })
    );
  }

  localized = localized.replace(
    COMPLETED_STATUS_LINE_PATTERN,
    (_match, linePrefix: string) =>
      `${linePrefix}${t('workflow.generatedText.completedStatus', {
        defaultValue: 'Status: Completed',
      })}`
  );

  localized = replaceExactGeneratedMessages(
    localized,
    USER_APPROVED_STEP_RESULT_MESSAGES,
    t('workflow.generatedText.userApprovedStepResult', {
      defaultValue: USER_APPROVED_STEP_RESULT,
    })
  );

  return replaceExactGeneratedMessages(
    localized,
    USER_APPROVED_LOOP_RESULT_MESSAGES,
    t('workflow.generatedText.userApprovedLoopResult', {
      defaultValue: 'User approved the loop result.',
    })
  );
}
