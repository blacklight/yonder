<script setup lang="ts">
import { ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { ApiError } from "@/api/errors";
import {
  EMAIL_TEMPLATE_KINDS,
  getEmailTemplate,
  putEmailTemplate,
  sendTestEmail,
  type EmailTemplate,
} from "@/api/adminEmail";
import Button from "@/components/ui/Button.vue";
import Input from "@/components/ui/Input.vue";
import ConfirmDialog from "@/components/admin/ConfirmDialog.vue";
import AdminSettingsPage from "./AdminSettingsPage.vue";

const { t } = useI18n();

const kind = ref<string>(EMAIL_TEMPLATE_KINDS[0]);
const template = ref<EmailTemplate | null>(null);
const original = ref<EmailTemplate | null>(null);
const tplError = ref("");
const tplSaved = ref(false);
const tplPending = ref(false);
const switchTarget = ref<string | null>(null);
let loadToken = 0;

// Guard against out-of-order responses when the kind is switched
// quickly, and confirm before discarding unsaved edits.
watch(kind, (next, prev) => {
  if (
    original.value &&
    template.value &&
    (template.value.subject !== original.value.subject ||
      template.value.text_body !== original.value.text_body ||
      template.value.html_body !== original.value.html_body)
  ) {
    kind.value = prev;
    switchTarget.value = next;
    return;
  }
  void loadTemplate(next);
});

async function loadTemplate(k: string) {
  const token = ++loadToken;
  template.value = null;
  original.value = null;
  tplError.value = "";
  try {
    const fetched = await getEmailTemplate(k);
    if (token !== loadToken) return;
    template.value = { ...fetched };
    original.value = { ...fetched };
  } catch (e) {
    if (token === loadToken)
      tplError.value = e instanceof ApiError ? e.message : String(e);
  }
}

void loadTemplate(kind.value);

function confirmSwitch() {
  const target = switchTarget.value;
  switchTarget.value = null;
  if (target) {
    kind.value = target;
    void loadTemplate(target);
  }
}

async function saveTemplate() {
  if (!template.value) return;
  tplPending.value = true;
  tplError.value = "";
  tplSaved.value = false;
  try {
    template.value = await putEmailTemplate(kind.value, {
      subject: template.value.subject,
      text_body: template.value.text_body,
      html_body: template.value.html_body,
    });
    tplSaved.value = true;
  } catch (e) {
    tplError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    tplPending.value = false;
  }
}

const testTo = ref("");
const testMsg = ref("");
const testPending = ref(false);

async function sendTest() {
  testPending.value = true;
  testMsg.value = "";
  try {
    await sendTestEmail(testTo.value.trim() || undefined);
    testMsg.value = t("admin.email.testSent");
  } catch (e) {
    testMsg.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    testPending.value = false;
  }
}
</script>

<template>
  <AdminSettingsPage title-key="admin.nav.email" :sections="['email']">
    <section class="email-extra email-extra--templates">
      <h3 class="email-extra__title">{{ t("admin.email.templates") }}</h3>
      <label class="email-extra__row">
        <span>{{ t("admin.email.templateLabel") }}</span>
        <select v-model="kind">
          <option v-for="k in EMAIL_TEMPLATE_KINDS" :key="k" :value="k">
            {{ t(`admin.email.kinds.${k}`) }}
          </option>
        </select>
      </label>
      <template v-if="template">
        <Input v-model="template.subject" :label="t('admin.email.subject')" />
        <label class="email-extra__row email-extra__row--top">
          <span>{{ t("admin.email.textBody") }}</span>
          <textarea v-model="template.text_body" rows="6" spellcheck="false" />
        </label>
        <label class="email-extra__row email-extra__row--top">
          <span>{{ t("admin.email.htmlBody") }}</span>
          <textarea v-model="template.html_body" rows="6" spellcheck="false" />
        </label>
        <div class="email-extra__row">
          <Button :loading="tplPending" @click="saveTemplate">
            {{ t("common.save") }}
          </Button>
          <span v-if="tplSaved" class="email-extra__saved">
            {{ t("admin.settings.saved") }}
          </span>
        </div>
      </template>
      <p v-if="tplError" class="email-extra__error" role="alert">
        {{ tplError }}
      </p>
    </section>

    <section class="email-extra email-extra--test">
      <h3 class="email-extra__title">{{ t("admin.email.test") }}</h3>
      <div class="email-extra__row">
        <Input
          v-model="testTo"
          :label="t('admin.email.testTo')"
          type="email"
          :placeholder="t('admin.email.testToHint')"
        />
        <Button variant="secondary" :loading="testPending" @click="sendTest">
          {{ t("admin.email.testSend") }}
        </Button>
      </div>
      <p v-if="testMsg" class="email-extra__hint">{{ testMsg }}</p>
    </section>

    <ConfirmDialog
      v-if="switchTarget"
      :title="t('admin.email.discardTitle')"
      :message="t('admin.email.discardChanges')"
      danger
      @confirm="confirmSwitch"
      @cancel="switchTarget = null"
    />
  </AdminSettingsPage>
</template>

<style scoped>
.email-extra {
  margin-top: var(--space-6);
}
.email-extra__title {
  margin: 0 0 var(--space-2);
  padding-bottom: var(--space-2);
  border-bottom: 1px solid var(--border);
  font-size: var(--text-xs);
  font-weight: 700;
  letter-spacing: 0.05em;
  text-transform: uppercase;
  color: var(--text-muted);
}
.email-extra__row {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  margin-bottom: var(--space-3);
}
.email-extra__row--top {
  align-items: start;
}
.email-extra--test .email-extra__row {
  align-items: end;
}
.email-extra--templates {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.email-extra--templates label span {
  width: 10rem;
}
.email-extra__row textarea {
  flex: 1;
  max-width: 32rem;
  padding: var(--space-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  background: var(--bg-raised);
  color: var(--text);
  font: inherit;
}
@media (max-width: 768px) {
  .email-extra--templates .email-extra__row {
    display: flex;
    flex-direction: column;
    align-items: start;
    gap: var(--space-2);
  }

  .email-extra--templates textarea {
    width: 100%;
  }
}
.email-extra__saved,
.email-extra__hint {
  font-size: var(--text-xs);
  color: var(--accent-link);
}
.email-extra__error {
  font-size: var(--text-xs);
  color: var(--danger);
}
</style>
