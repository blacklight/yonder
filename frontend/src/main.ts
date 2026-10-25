import { createApp } from "vue";
import { createPinia } from "pinia";
import App from "./App.vue";
import { router } from "./router";
import { i18n, loadInitialLocale } from "./i18n";
import "./styles/tokens.css";
import "./styles/base.css";
import "./styles/rtl.css";
import { usePwa } from "./composables/usePwa";

async function bootstrap() {
  await loadInitialLocale();
  const app = createApp(App);
  app.use(createPinia());
  app.use(router);
  app.use(i18n);
  app.mount("#app");
  usePwa().registerServiceWorker();
}

void bootstrap();
