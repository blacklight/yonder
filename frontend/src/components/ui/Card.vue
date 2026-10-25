<template>
  <section class="card">
    <header v-if="$slots.header" class="card__header">
      <slot name="header" />
    </header>
    <div class="card__body"><slot /></div>
    <footer v-if="$slots.footer" class="card__footer">
      <slot name="footer" />
    </footer>
  </section>
</template>

<style scoped>
.card {
  background: var(--bg-raised);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
  padding: var(--space-4);
}
.card__header {
  margin-bottom: var(--space-3);
  font-weight: 600;
}
.card__footer {
  margin-top: var(--space-3);
}
</style>
