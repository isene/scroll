/// A browser tab with URL, content, history, links, forms, images
#[derive(Clone)]
pub struct Tab {
    pub url: String,
    pub title: String,
    pub content: String,      // Rendered text (with ANSI codes)
    pub ix: usize,            // Scroll position
    pub links: Vec<Link>,
    pub forms: Vec<Form>,
    pub images: Vec<ImageRef>,
    pub back_history: Vec<HistoryEntry>,
    pub forward_history: Vec<HistoryEntry>,
    pub site_bg: Option<u8>,
    pub site_fg: Option<u8>,
}

#[derive(Clone)]
pub struct Link {
    pub index: usize,
    pub href: String,
    pub text: String,
    pub line: usize,
}

#[derive(Clone)]
pub struct Form {
    pub action: String,
    pub method: String,
    pub fields: Vec<FormField>,
    pub line: usize,
}

#[derive(Clone)]
pub struct FormField {
    pub field_type: String,
    pub name: String,
    pub value: String,
    pub placeholder: String,
    pub options: Vec<(String, String)>, // for select: (value, label)
    pub line: usize,
}

#[derive(Clone)]
pub struct ImageRef {
    pub src: String,
    pub alt: String,
    pub line: usize,
    pub height: usize,
}

#[derive(Clone)]
pub struct HistoryEntry {
    pub url: String,
    pub ix: usize,
}

impl Tab {
    pub fn new(url: &str) -> Self {
        Tab {
            url: url.to_string(),
            title: String::new(),
            content: String::new(),
            ix: 0,
            links: Vec::new(),
            forms: Vec::new(),
            images: Vec::new(),
            back_history: Vec::new(),
            forward_history: Vec::new(),
            site_bg: None,
            site_fg: None,
        }
    }

    pub fn navigate(&mut self, url: &str) {
        // Push current to back history
        if !self.url.is_empty() && self.url != "about:blank" {
            self.back_history.push(HistoryEntry {
                url: self.url.clone(),
                ix: self.ix,
            });
        }
        self.forward_history.clear();
        self.url = url.to_string();
        self.ix = 0;
        self.links.clear();
        self.forms.clear();
        self.images.clear();
        self.site_bg = None;
        self.site_fg = None;
    }

    pub fn go_back(&mut self) -> Option<String> {
        let entry = self.back_history.pop()?;
        self.forward_history.push(HistoryEntry {
            url: self.url.clone(),
            ix: self.ix,
        });
        self.url = entry.url.clone();
        self.ix = entry.ix;
        Some(entry.url)
    }

    pub fn go_forward(&mut self) -> Option<String> {
        let entry = self.forward_history.pop()?;
        self.back_history.push(HistoryEntry {
            url: self.url.clone(),
            ix: self.ix,
        });
        self.url = entry.url.clone();
        self.ix = entry.ix;
        Some(entry.url)
    }

    pub fn can_go_back(&self) -> bool { !self.back_history.is_empty() }
    pub fn can_go_forward(&self) -> bool { !self.forward_history.is_empty() }
}
