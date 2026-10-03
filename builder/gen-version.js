const fs = require('fs');
const path = require('path');

const root = path.join(__dirname, '..');
const { version } = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'));

const content = `// Generated from package.json by builder/gen-version.js. Do not edit by hand.
// A macro, not a const, so \`concat!\` can append the NUL the C ABI returns from ic_plugin_version.
macro_rules! plugins_version {
    () => {
        "${version}"
    };
}
`;

const out = path.join(root, 'version.rs');
fs.writeFileSync(out, content, 'utf8');
console.log(`Generated version.rs -> v${version}`);
