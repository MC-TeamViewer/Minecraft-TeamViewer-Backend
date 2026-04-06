<script setup lang="ts">
import ElAlert from "element-plus/es/components/alert/index";
import ElButton from "element-plus/es/components/button/index";
import ElCard from "element-plus/es/components/card/index";
import ElInput from "element-plus/es/components/input/index";
import { reactive, ref } from "vue";

const props = defineProps<{
  loading?: boolean;
  errorMessage?: string | null;
}>();

const emit = defineEmits<{
  login: [payload: { username: string; password: string }];
}>();

const form = reactive({
  username: "admin",
  password: "",
});
const localError = ref<string | null>(null);

function submit() {
  if (!form.username.trim() || !form.password) {
    localError.value = "请输入用户名和密码";
    return;
  }
  localError.value = null;
  emit("login", {
    username: form.username.trim(),
    password: form.password,
  });
}
</script>

<template>
  <div class="login-shell">
    <el-card shadow="never" class="surface-card login-card">
      <div class="hero-eyebrow">管理登录</div>
      <h1>TeamViewRelay Admin</h1>
      <p class="login-copy">登录后查看在线概况、实时网速、历史流量与审计日志。</p>

      <el-alert
        v-if="errorMessage || localError"
        type="error"
        :closable="false"
        show-icon
        :title="errorMessage || localError || ''"
        class="login-alert"
      />

      <div class="login-form">
        <el-input
          :model-value="form.username"
          placeholder="用户名"
          autocomplete="username"
          @update:model-value="(value: string) => { form.username = value; }"
        />
        <el-input
          :model-value="form.password"
          type="password"
          show-password
          placeholder="密码"
          autocomplete="current-password"
          @keydown.enter="submit"
          @update:model-value="(value: string) => { form.password = value; }"
        />
        <el-button type="primary" :loading="loading" @click="submit">登录后台</el-button>
      </div>
    </el-card>
  </div>
</template>
