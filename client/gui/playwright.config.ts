import { defineConfig } from '@playwright/test';
export default defineConfig({
  testDir: './tests',
  use: { baseURL: 'http://127.0.0.1:1420', browserName: 'chromium', channel: 'msedge', locale: 'ru-RU' },
  webServer: { command: 'npm run dev', url: 'http://127.0.0.1:1420', reuseExistingServer: false },
  reporter: 'list',
});
