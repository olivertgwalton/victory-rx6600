"""wineserver: mapped views in a red-black tree by base address as well as
the list, so map, unmap and address lookups stop walking every view."""
def sub(p, o, n, count=1):
    s = open(p).read()
    assert s.count(o) == count, (p, s.count(o), o[:60])
    open(p, "w").write(s.replace(o, n))

# process.h: the tree beside the list.
s = open("server/process.h").read()
if '#include "wine/rbtree.h"' not in s:
    s = s.replace('#include "object.h"', '#include "object.h"\n#include "wine/rbtree.h"', 1)
    assert '#include "wine/rbtree.h"' in s
s = s.replace("    struct list          views;           /* list of memory views */",
              "    struct list          views;           /* list of memory views */\n"
              "    struct rb_tree       view_tree;       /* the same views by base address */", 1)
assert "view_tree" in s
open("server/process.h", "w").write(s)

# file.h: the tree's setup, used by process.c.
sub("server/file.h", "extern struct memory_view *find_mapped_view( struct process *process, client_ptr_t base );",
    "extern struct memory_view *find_mapped_view( struct process *process, client_ptr_t base );\n"
    "extern void init_process_views( struct process *process );")

# process.c
sub("server/process.c", "    list_init( &process->views );\n", "    list_init( &process->views );\n    init_process_views( process );\n")

# mapping.c
m = "server/mapping.c"
sub(m, """struct memory_view
{
    struct list     entry;           /* entry in per-process view list */""", """struct memory_view
{
    struct list     entry;           /* entry in per-process view list */
    struct rb_entry tree_entry;      /* entry in the per-process tree by base */
    struct process *owner;           /* process whose tree holds it (NULL: none) */""")

sub(m, """/* find a memory view from its base address */
struct memory_view *find_mapped_view( struct process *process, client_ptr_t base )
{
    struct memory_view *view;

    LIST_FOR_EACH_ENTRY( view, &process->views, struct memory_view, entry )
        if (view->base == base) return view;

    set_error( STATUS_NOT_MAPPED_VIEW );
    return NULL;
}

/* find a memory view from any address inside it */
static struct memory_view *find_mapped_addr( struct process *process, client_ptr_t addr )
{
    struct memory_view *view;

    LIST_FOR_EACH_ENTRY( view, &process->views, struct memory_view, entry )
        if (addr >= view->base && addr < view->base + view->size) return view;

    set_error( STATUS_NOT_MAPPED_VIEW );
    return NULL;
}

/* check if an address range is valid for creating a view */
static int is_valid_view_addr( struct process *process, client_ptr_t addr, mem_size_t size )
{
    struct memory_view *view;

    if (!size) return 0;
    if (addr & host_page_mask) return 0;
    if (addr + size < addr) return 0;  /* overflow */

    /* check for overlapping view */
    LIST_FOR_EACH_ENTRY( view, &process->views, struct memory_view, entry )
    {
        if (view->base + view->size <= addr) continue;
        if (view->base >= addr + size) continue;
        return 0;
    }
    return 1;
}""", """static int compare_view_base( const void *key, const struct rb_entry *entry )
{
    client_ptr_t base = *(const client_ptr_t *)key;
    const struct memory_view *view = RB_ENTRY_VALUE( entry, const struct memory_view, tree_entry );

    if (base < view->base) return -1;
    if (base > view->base) return 1;
    return 0;
}

void init_process_views( struct process *process )
{
    rb_init( &process->view_tree, compare_view_base );
}

/* the view with the highest base below `limit` (views do not overlap) */
static struct memory_view *view_below( struct process *process, client_ptr_t limit )
{
    struct rb_entry *node = process->view_tree.root;
    struct memory_view *found = NULL;

    while (node)
    {
        struct memory_view *view = RB_ENTRY_VALUE( node, struct memory_view, tree_entry );
        if (view->base < limit)
        {
            found = view;
            node = node->right;
        }
        else node = node->left;
    }
    return found;
}

/* find a memory view from its base address */
struct memory_view *find_mapped_view( struct process *process, client_ptr_t base )
{
    struct rb_entry *entry = rb_get( &process->view_tree, &base );

    if (entry) return RB_ENTRY_VALUE( entry, struct memory_view, tree_entry );
    set_error( STATUS_NOT_MAPPED_VIEW );
    return NULL;
}

/* find a memory view from any address inside it */
static struct memory_view *find_mapped_addr( struct process *process, client_ptr_t addr )
{
    struct memory_view *view = addr + 1 ? view_below( process, addr + 1 ) : NULL;

    if (view && addr < view->base + view->size) return view;
    set_error( STATUS_NOT_MAPPED_VIEW );
    return NULL;
}

/* check if an address range is valid for creating a view */
static int is_valid_view_addr( struct process *process, client_ptr_t addr, mem_size_t size )
{
    struct memory_view *view;

    if (!size) return 0;
    if (addr & host_page_mask) return 0;
    if (addr + size < addr) return 0;  /* overflow */

    /* check for overlapping view: only the last one starting before the
     * range's end can overlap it, as views do not overlap each other */
    view = view_below( process, addr + size );
    return !view || view->base + view->size <= addr;
}""")

sub(m, """            process->image_info = view->image;
            list_add_head( &process->views, &view->entry );
            return 1;
        }
    }
    list_add_tail( &process->views, &view->entry );
    return 0;
}""", """            process->image_info = view->image;
            list_add_head( &process->views, &view->entry );
            if (!rb_put( &process->view_tree, &view->base, &view->tree_entry )) view->owner = process;
            return 1;
        }
    }
    list_add_tail( &process->views, &view->entry );
    if (!rb_put( &process->view_tree, &view->base, &view->tree_entry )) view->owner = process;
    return 0;
}""")

sub(m, """    if (view->shared) release_object( view->shared );
    list_remove( &view->entry );
    free( view );""", """    if (view->shared) release_object( view->shared );
    if (view->owner) rb_remove( &view->owner->view_tree, &view->tree_entry );
    list_remove( &view->entry );
    free( view );""")
print("ok")
