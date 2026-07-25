use std::io::{Write, stdout,Result};
use std::collections::BTreeMap;

use crossterm::{queue, execute};
use crossterm::terminal::{size, Clear, ClearType, disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::cursor::{MoveTo, Hide, Show};
use crossterm::style::Stylize;

pub struct TerminalGuard;
impl TerminalGuard {
    pub fn new() -> Result<Self> {
        enable_raw_mode()?;
        execute!(stdout(), EnterAlternateScreen, Hide)?;
        Ok(Self)
    }

    pub fn leave_terminal(&self) -> Result<()> {
        execute!(stdout(), Show, LeaveAlternateScreen)?;
        disable_raw_mode()?;
        Ok(())
    }

    /// 重新接管终端
    pub fn enter_terminal(&self) -> Result<()> {
        enable_raw_mode()?;
        execute!(stdout(), EnterAlternateScreen, Hide)?;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}
pub struct Block {
    x:u8,
    dx:i16,
    y:u8,
    dy:i16,

    l:u16,
    //w:u16,

    latest: [u8; 32],
    str_len: usize,
}

impl Block {
    pub fn new(x:u8, dx:i16, y:u8, dy:i16, l:u16) -> Self{
        Self{
            x:x,
            dx:dx,
            y:y,
            dy:dy,
            l:l,
            //w:w,
            latest:[0; 32],
            str_len:0,
        }
    }

    fn update(&mut self,text:String){
        self.str_len = text.as_bytes().len().min(31);
        self.latest[0..self.str_len].copy_from_slice(&text.as_bytes()[0..self.str_len]);
    }

    fn get_latest(&self) -> &str {
        std::str::from_utf8(&self.latest[0..self.str_len]).unwrap()
    }
}

pub struct Writer {
    cols:u16,
    rows:u16,
    blocks:Vec<Block>,
    selector:u16,
    height:u16,
    _layout:String,
}
impl Writer {
    pub fn start(out: &mut impl Write,layout:String) -> Self {
        let (cols,rows) = size().unwrap_or((80,24));
        //从layout获取
        let mut writer = Self {
            cols:cols,
            rows:rows,
            blocks:Vec::new(),
            selector:0,
            height:layout.trim().parse().unwrap_or(2) + 2,
            _layout:layout,
        };
        writer.print_background(out);

        writer
    }

    pub fn add_block(&mut self,block: Block){
        self.blocks.push(block);
    }

    pub fn get_sort(&mut self) -> Vec<Vec<usize>>{
        let mut groups: BTreeMap<(u8, i16), Vec<usize>> = BTreeMap::new();

        for (idx, block) in self.blocks.iter().enumerate() {
            groups.entry((block.y, block.dy)).or_default().push(idx);
        }

        let mut result = Vec::with_capacity(groups.len());
        for (_, mut indices) in groups {
            indices.sort_by_key(|&i| self.blocks[i].x);
            result.push(indices);
        }
        self.selector = result[0][0] as u16;
        result
    }

    pub fn update_all(&mut self,out: &mut impl Write){
        //全量绘制
        self.print_background(out);
        for i in 0..self.blocks.len(){
            print_block(out,self.cols,self.rows,&self.blocks[i],i == self.selector as usize);
        }
    }

    fn print_background(&mut self,out: &mut impl Write) {
        //从layout获取background的内容

        let content_width = self.cols.saturating_sub(2) as usize;
        let border = "─".repeat(content_width);
        let context = " ".repeat(content_width);

        let _ = queue!(out,Clear(ClearType::All),MoveTo(0,0));
        let _ = writeln!(out, "╭{border}╮\r");
        let _ = writeln!(out, "│{context}│\r");
        let _ = writeln!(out, "│{context}│\r");
        let _ = writeln!(out, "╰{border}╯\r");
    }

    pub fn update_block(&mut self,out: &mut impl Write, i:usize, latest:String){
        if  self.blocks[i].get_latest() == latest{
            return;
        }
        self.blocks[i].update(latest);
        print_block(out,self.cols,self.rows,&self.blocks[i],i == self.selector as usize);
    }

    pub fn check_size(&mut self,out: &mut impl Write){
        let (cols,rows) = size().unwrap_or((80,24));
        if cols != self.cols || rows != self.rows{
            let _ = queue!(out,Clear(ClearType::All),MoveTo(0,0));
            self.cols = cols;
            self.rows = rows;
            self.update_all(out);
        }
    }

    pub fn set_selector(&mut self,out: &mut impl Write,selector:u16){
        print_block(out,self.cols,self.rows,&self.blocks[self.selector as usize],false);
        self.selector = selector;
        print_block(out,self.cols,self.rows,&self.blocks[self.selector as usize],true);
    }

    pub fn get_selector(&self) -> u16{
        self.selector
    }
}

// 局部打印（安全UTF-8截断，零堆分配）
fn print_block(out: &mut impl Write, cols:u16, rows:u16, block: &Block, is_selected: bool) {
    // x 坐标
    let base_x = block.x as i32 * cols as i32 / 100;
    let final_x = (base_x + block.dx as i32).max(0) as u16;

    // y 坐标
    let base_y = block.y as i32 * rows as i32 / 100;
    let final_y = (base_y + block.dy as i32).max(0) as u16;
    let _ = queue!(out, MoveTo(final_x,final_y));

    let l_max = (cols - final_x) as usize;
    let l = (block.l as usize).min(l_max);

    if l == 0 {
        return;
    }
    
    // 安全截断UTF-8字符串，不产生堆分配
    let mut chars = block.get_latest().chars();
    let mut byte_len = 0;
    for _ in 0..l {
        match chars.next() {
            Some(c) => byte_len += c.len_utf8(),
            None => break,
        }
    }
    
    let truncated = &block.get_latest()[..byte_len];
    if is_selected {
        let _ = write!(out, "{:l$}", truncated.underlined());
    } else {
        let _ = write!(out, "{:l$}", truncated);
    }
}