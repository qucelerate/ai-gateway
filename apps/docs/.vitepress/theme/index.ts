import { h } from 'vue'
import DefaultTheme from 'vitepress/theme'
import MarkdownActions from './MarkdownActions.vue'
import './custom.css'

export default {
  extends: DefaultTheme,
  // Above each doc page's content: copy or open its Markdown copy.
  Layout: () => h(DefaultTheme.Layout, null, { 'doc-before': () => h(MarkdownActions) })
}
