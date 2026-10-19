const fs = require('fs');
const v = process.env.INPUT_VALUE;
console.log(`node got ${v}`);
fs.appendFileSync(process.env.GITHUB_OUTPUT, `doubled=${v}${v}\n`);
fs.appendFileSync(process.env.GITHUB_STATE, `saved=${v}\n`);
