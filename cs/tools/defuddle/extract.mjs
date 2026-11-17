import fs from 'node:fs';
import { parseHTML } from 'linkedom';
import { Defuddle } from 'defuddle/node';

const [path, url] = process.argv.slice(2);
if (!path || !url) throw new Error('usage: extract.mjs HTML_PATH URL');
const html = fs.readFileSync(path, 'utf8');
const { document } = parseHTML(html);
const result = await Defuddle(document, url, {
  markdown: true,
  useAsync: false,
});
process.stdout.write(JSON.stringify({
  content: result.content,
  metadata: {
    title: result.title || '',
    author: result.author || '',
    description: result.description || '',
    published: result.published || '',
    site: result.site || '',
    language: result.language || '',
    word_count: result.wordCount || 0,
  },
}));
