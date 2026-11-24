// Lynx operator admin panel (admin/web): its own app with the Activity panel's look (Tailwind tokens,
// fonts) but none of its Discord code: no SDK, no Activity session.
import '@fontsource-variable/inter'
import '@fontsource/jetbrains-mono/400.css'
import './index.css'
import './i18n'
import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import AdminApp from './App'

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <AdminApp />
  </StrictMode>,
)
