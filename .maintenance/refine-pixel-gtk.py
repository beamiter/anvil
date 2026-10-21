from pathlib import Path
p = Path('src/vim9_ide_buffer.rs')
s = p.read_text()
old = '        let Some(position) = self.source_at(x, y) else { return false; };'
new = '''        let Some(position) = self.direct_source_at(x, y) else { return false; };'''
assert s.count(old) == 1
s = s.replace(old, new)
old = '    fn press_source(&self, session: &Vim9Session, x: f64, y: f64) -> bool {'
new = '''    fn direct_source_at(&self, x: f64, y: f64) -> Option<Vim9BufferPosition> {
        let position = self.source_at(x, y)?;
        // Do not replace stationary Vim mouse behavior. Source transactions
        // are necessary only while precision presentation differs from its
        // integer grid; plain clicks/multi-clicks otherwise remain Vim-owned.
        if !self.imp().pixels.borrow().contains_key(&position.window) { return None; }
        let presented = self.imp().presented.borrow();
        let offset = presented.as_ref()?.offsets.get(&position.window).copied()?;
        (offset != 0.0).then_some(position)
    }

    fn press_source(&self, session: &Vim9Session, x: f64, y: f64) -> bool {'''
assert s.count(old) == 1
s = s.replace(old, new)
old = '        assert_eq!(surface.source_at(9.0, 75.0).unwrap().byte_column, 1);'
new = old + '''
        assert_eq!(surface.direct_source_at(9.0, 75.0), surface.source_at(9.0, 75.0));'''
assert s.count(old) == 1
s = s.replace(old, new)
old = '        assert_eq!(surface.presented_cursor().unwrap().y, 20.0);'
new = old + '''
        assert!(surface.direct_source_at(9.0, 75.0).is_none(), "stationary input remains Vim-owned");'''
assert s.count(old) == 1
p.write_text(s.replace(old, new))
print('Scoped source pointer ownership to actual precision offsets')
